#!/bin/bash

echo "building pd..."
mkdir -p ${DIST_PATH}
mkdir -p ${BIN_PATH}

rm -rf ${DIST_PATH}/src/github.com/pingcap/pd
git clone --depth=1 https://github.com/pingcap/pd.git ${DIST_PATH}/src/github.com/pingcap/pd

cd ${DIST_PATH}/src/github.com/pingcap/pd
export GOPATH=$DIST_PATH
make build
cp -f ./bin/pd-server $BIN_PATH