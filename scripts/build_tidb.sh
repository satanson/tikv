#!/bin/bash

echo "building tidb..."
mkdir -p ${DIST_PATH}
mkdir -p ${BIN_PATH}

rm -rf ${DIST_PATH}/src/github.com/pingcap/tidb
git clone --depth=1 https://github.com/pingcap/tidb.git ${DIST_PATH}/src/github.com/pingcap/tidb

cd ${DIST_PATH}/src/github.com/pingcap/tidb
export GOPATH=$DIST_PATH
make server
cp -f ./tidb-server/tidb-server $BIN_PATH