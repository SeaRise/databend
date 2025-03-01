// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use common_meta_raft_store::state_machine::testing::pretty_snapshot;
use common_meta_raft_store::state_machine::testing::snapshot_logs;
use common_meta_raft_store::state_machine::SerializableSnapshot;
use common_meta_sled_store::openraft::async_trait::async_trait;
use common_meta_sled_store::openraft::storage::Adaptor;
use common_meta_sled_store::openraft::storage::RaftLogReaderExt;
use common_meta_sled_store::openraft::testing::StoreBuilder;
use common_meta_sled_store::openraft::RaftSnapshotBuilder;
use common_meta_sled_store::openraft::RaftStorage;
use common_meta_types::new_log_id;
use common_meta_types::CommittedLeaderId;
use common_meta_types::Entry;
use common_meta_types::EntryPayload;
use common_meta_types::LogId;
use common_meta_types::Membership;
use common_meta_types::StorageError;
use common_meta_types::StoredMembership;
use common_meta_types::TypeConfig;
use common_meta_types::Vote;
use common_tracing::func_name;
use databend_meta::meta_service::raftmeta::LogStore;
use databend_meta::meta_service::raftmeta::SMStore;
use databend_meta::store::RaftStore;
use databend_meta::Opened;
use log::debug;
use log::info;
use maplit::btreeset;
use minitrace::prelude::*;
use pretty_assertions::assert_eq;
use test_harness::test;

use crate::testing::meta_service_test_harness;
use crate::testing::meta_service_test_harness_sync;
use crate::tests::service::MetaSrvTestContext;

struct MetaStoreBuilder {}

#[async_trait]
impl StoreBuilder<TypeConfig, LogStore, SMStore, MetaSrvTestContext> for MetaStoreBuilder {
    async fn build(&self) -> Result<(MetaSrvTestContext, LogStore, SMStore), StorageError> {
        let tc = MetaSrvTestContext::new(555);
        let sto = RaftStore::open_create(&tc.config.raft_config, None, Some(()))
            .await
            .expect("fail to create store");
        let (log_store, sm_store) = Adaptor::new(sto);
        Ok((tc, log_store, sm_store))
    }
}

#[test(harness = meta_service_test_harness_sync)]
#[minitrace::trace]
fn test_impl_raft_storage() -> anyhow::Result<()> {
    let root = Span::root(func_name!(), SpanContext::random());
    let _guard = root.set_local_parent();

    common_meta_sled_store::openraft::testing::Suite::test_all(MetaStoreBuilder {})?;

    Ok(())
}

#[test(harness = meta_service_test_harness)]
#[minitrace::trace]
async fn test_meta_store_restart() -> anyhow::Result<()> {
    // - Create a meta store
    // - Update meta store
    // - Close and reopen it
    // - Test state is restored: hard state, log, state machine

    let id = 3;
    let tc = MetaSrvTestContext::new(id);

    info!("--- new meta store");
    {
        let mut sto = RaftStore::open_create(&tc.config.raft_config, None, Some(())).await?;
        assert_eq!(id, sto.id);
        assert!(!sto.is_opened());
        assert_eq!(None, sto.read_vote().await?);

        info!("--- update metasrv");

        sto.save_vote(&Vote::new(10, 5)).await?;

        sto.append_to_log([Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 2), 1),
            payload: EntryPayload::Blank,
        }])
        .await?;

        sto.apply_to_state_machine(&[Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 2), 2),
            payload: EntryPayload::Blank,
        }])
        .await?;
    }

    info!("--- reopen meta store");
    {
        let mut sto = RaftStore::open_create(&tc.config.raft_config, Some(()), None).await?;
        assert_eq!(id, sto.id);
        assert!(sto.is_opened());
        assert_eq!(Some(Vote::new(10, 5)), sto.read_vote().await?);

        assert_eq!(
            LogId::new(CommittedLeaderId::new(1, 2), 1),
            sto.get_log_id(1).await?
        );
        assert_eq!(
            Some(LogId::new(CommittedLeaderId::new(1, 2), 2)),
            sto.last_applied_state().await?.0
        );
    }
    Ok(())
}

#[test(harness = meta_service_test_harness)]
#[minitrace::trace]
async fn test_meta_store_build_snapshot() -> anyhow::Result<()> {
    // - Create a metasrv
    // - Apply logs
    // - Create a snapshot check snapshot state

    let id = 3;
    let tc = MetaSrvTestContext::new(id);

    let mut sto = RaftStore::open_create(&tc.config.raft_config, None, Some(())).await?;

    info!("--- feed logs and state machine");

    let (logs, want) = snapshot_logs();

    sto.log.append(logs.clone()).await?;
    for l in logs.iter() {
        sto.state_machine.write().await.apply(l).await?;
    }

    let curr_snap = sto.build_snapshot().await?;
    assert_eq!(Some(new_log_id(1, 0, 9)), curr_snap.meta.last_log_id);

    info!("--- check snapshot");
    {
        let data = curr_snap.snapshot.into_inner();

        let ser_snap: SerializableSnapshot = serde_json::from_slice(&data)?;
        let res = pretty_snapshot(&ser_snap.kvs);
        debug!("res: {:?}", res);

        assert_eq!(want, res);
    }

    Ok(())
}

#[test(harness = meta_service_test_harness)]
#[minitrace::trace]
async fn test_meta_store_current_snapshot() -> anyhow::Result<()> {
    // - Create a metasrv
    // - Apply logs
    // - Create a snapshot check snapshot state

    let id = 3;
    let tc = MetaSrvTestContext::new(id);

    let mut sto = RaftStore::open_create(&tc.config.raft_config, None, Some(())).await?;

    info!("--- feed logs and state machine");

    let (logs, want) = snapshot_logs();

    sto.log.append(logs.clone()).await?;
    for l in logs.iter() {
        sto.state_machine.write().await.apply(l).await?;
    }

    sto.build_snapshot().await?;

    info!("--- check get_current_snapshot");

    let curr_snap = sto.get_current_snapshot().await?.unwrap();
    assert_eq!(Some(new_log_id(1, 0, 9)), curr_snap.meta.last_log_id);

    info!("--- check snapshot");
    {
        let data = curr_snap.snapshot.into_inner();

        let ser_snap: SerializableSnapshot = serde_json::from_slice(&data)?;
        let res = pretty_snapshot(&ser_snap.kvs);
        debug!("res: {:?}", res);

        assert_eq!(want, res);
    }

    Ok(())
}

#[test(harness = meta_service_test_harness)]
#[minitrace::trace]
async fn test_meta_store_install_snapshot() -> anyhow::Result<()> {
    // - Create a metasrv
    // - Feed logs
    // - Create a snapshot
    // - Create a new metasrv and restore it by install the snapshot

    let (logs, want) = snapshot_logs();

    let id = 3;
    let snap;
    {
        let tc = MetaSrvTestContext::new(id);

        let mut sto = RaftStore::open_create(&tc.config.raft_config, None, Some(())).await?;

        info!("--- feed logs and state machine");

        sto.log.append(logs.clone()).await?;
        for l in logs.iter() {
            sto.state_machine.write().await.apply(l).await?;
        }
        snap = sto.build_snapshot().await?;
    }

    let data = snap.snapshot.into_inner();

    info!("--- reopen a new metasrv to install snapshot");
    {
        let tc = MetaSrvTestContext::new(id);

        let mut sto = RaftStore::open_create(&tc.config.raft_config, None, Some(())).await?;

        info!("--- rejected because old sm is not cleaned");
        {
            sto.raft_state.write_state_machine_id(&(1, 2)).await?;
            let res = sto.do_install_snapshot(&data).await;
            assert!(res.is_err(), "different ids disallow installing snapshot");
            assert!(
                res.unwrap_err()
                    .to_string()
                    .starts_with("another snapshot install is not finished yet: 1 2")
            );
        }

        info!("--- install snapshot");
        {
            sto.raft_state.write_state_machine_id(&(0, 0)).await?;
            sto.do_install_snapshot(&data).await?;
        }

        info!("--- check installed meta");
        {
            assert_eq!((1, 1), sto.raft_state.read_state_machine_id()?);

            let mem = sto.state_machine.write().await.get_membership()?;
            assert_eq!(
                Some(StoredMembership::new(
                    Some(LogId::new(CommittedLeaderId::new(1, 0), 5)),
                    Membership::new(vec![btreeset! {4,5,6}], ())
                )),
                mem
            );

            let last_applied = sto.state_machine.write().await.get_last_applied()?;
            assert_eq!(
                Some(LogId::new(CommittedLeaderId::new(1, 0), 9)),
                last_applied
            );
        }

        info!("--- check snapshot");
        {
            let curr_snap = sto.build_snapshot().await?;
            let data = curr_snap.snapshot.into_inner();

            let ser_snap: SerializableSnapshot = serde_json::from_slice(&data)?;
            let res = pretty_snapshot(&ser_snap.kvs);
            debug!("res: {:?}", res);

            assert_eq!(want, res);
        }
    }

    Ok(())
}
