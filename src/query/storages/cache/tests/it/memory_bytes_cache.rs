// Copyright 2022 Datafuse Labs.
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

use std::sync::Arc;

use common_base::base::tokio;
use common_base::base::uuid;
use common_exception::Result;
use common_storages_cache::CacheSettings;
use common_storages_cache::MemoryBytesCache;
use common_storages_cache::ObjectReaderWriter;
use opendal::services::fs;
use opendal::services::fs::Builder;
use opendal::Operator;

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
struct TestMeta {
    a: u64,
    b: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_memory_bytes_cache() -> Result<()> {
    let mut builder: Builder = fs::Builder::default();
    builder.root("/tmp");
    let op: Operator = Operator::new(builder.build()?);

    let path = uuid::Uuid::new_v4().to_string();
    let object = op.object(&path);

    // Cache.
    let settings = CacheSettings::default();
    let cache = Arc::new(MemoryBytesCache::create(&settings));

    let expect = Arc::new(serde_json::to_vec(&TestMeta { a: 0, b: 0 }).unwrap());

    // Reader Writer.
    let object_rw = ObjectReaderWriter::create(cache.clone());
    object_rw.write(&object, expect.clone()).await?;

    let actual: Arc<Vec<u8>> = object_rw.read(&object, 0, 16).await?;

    assert_eq!(actual, expect);

    // Remove.
    object_rw.remove(&object).await?;

    Ok(())
}
