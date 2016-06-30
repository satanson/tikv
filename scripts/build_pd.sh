#!/bin/bash

echo "building pd..."
rm -rf ${DEPS_PATH}/src/github.com/pingcap/pd
git clone --depth=1 https://github.com/pingcap/pd.git ${DEPS_PATH}/src/github.com/pingcap/pd

cd ${DEPS_PATH}/src/github.com/pingcap/pd
export GOPATH=$DEPS_PATH
make build
cp -f ./bin/pd-server $BIN_PATH