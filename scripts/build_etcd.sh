#!/bin/bash

echo "building etcd..."
mkdir -p ${DIST_PATH}
mkdir -p ${BIN_PATH}

rm -rf ${DIST_PATH}/src/github.com/coreos/etcd
git clone --depth=1 https://github.com/coreos/etcd.git ${DIST_PATH}/src/github.com/coreos/etcd

cd ${DIST_PATH}/src/github.com/coreos/etcd
export GOPATH=$DIST_PATH
./build
cp -f ./bin/etcd $BIN_PATH