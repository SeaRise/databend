#!/usr/bin/env bash

CURDIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$CURDIR"/../../../shell_env.sh

echo "DROP CATALOG IF EXISTS iceberg_ctl" | $MYSQL_CLIENT_CONNECT

## Create iceberg catalog
cat <<EOF | $MYSQL_CLIENT_CONNECT
CREATE CATALOG iceberg_ctl
TYPE=ICEBERG
CONNECTION=(
    URL='s3://testbucket/iceberg_ctl/'
    AWS_KEY_ID='minioadmin'
    AWS_SECRET_KEY='minioadmin'
    ENDPOINT_URL='${STORAGE_S3_ENDPOINT_URL}'
);
EOF

# Iceberg read is not ready on cluster yet
# echo "SELECT count(*) FROM iceberg_ctl.iceberg_db.iceberg_tbl;" | $MYSQL_CLIENT_CONNECT
