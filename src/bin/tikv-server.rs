// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(plugin)]
#![cfg_attr(feature = "dev", plugin(clippy))]

extern crate tikv;
extern crate getopts;
#[macro_use]
extern crate log;
extern crate rocksdb;
extern crate mio;
extern crate toml;
extern crate cadence;
extern crate libc;
extern crate fs2;

use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::io::Read;
use std::net::UdpSocket;

use getopts::{Options, Matches};
use rocksdb::{DB, Options as RocksdbOptions, BlockBasedOptions};
use mio::tcp::TcpListener;
use fs2::FileExt;
use cadence::{StatsdClient, NopMetricSink};

use tikv::storage::{Storage, Dsn, TEMP_DIR};
use tikv::util::{self, logger, panic_hook};
use tikv::util::metric::{self, NonblockUdpMetricSink};
use tikv::server::{DEFAULT_LISTENING_ADDR, SendCh, Server, Node, Config, bind, create_event_loop,
                   create_raft_storage};
use tikv::server::{ServerTransport, ServerRaftStoreRouter, MockRaftStoreRouter};
use tikv::server::{MockStoreAddrResolver, PdStoreAddrResolver};
use tikv::raftstore::store::{self, SnapManager};
use tikv::pd::{new_rpc_client, RpcClient};

const ROCKSDB_DSN: &'static str = "rocksdb";
const RAFTKV_DSN: &'static str = "raftkv";

fn print_usage(program: &str, opts: Options) {
    let brief = format!("Usage: {} [options]", program);
    print!("{}", opts.usage(&brief));
}

fn get_string_value<F>(short: &str,
                       long: &str,
                       matches: &Matches,
                       config: &toml::Value,
                       default: Option<String>,
                       f: F)
                       -> String
    where F: Fn(&toml::Value) -> Option<String>
{
    // avoid panic if short is not defined.
    let s = if matches.opt_defined(short) {
        matches.opt_str(short)
    } else {
        None
    };

    s.or_else(|| {
            config.lookup(long).and_then(|v| f(v)).or_else(|| {
                info!("malformed or missing {}, use default {:?}", long, default);
                default
            })
        })
        .expect(&format!("please specify {}", long))
}

fn get_integer_value<F>(short: &str,
                        long: &str,
                        matches: &Matches,
                        config: &toml::Value,
                        default: Option<i64>,
                        f: F)
                        -> i64
    where F: Fn(&toml::Value) -> Option<i64>
{
    let mut i = None;
    // avoid panic if short is not defined.
    if matches.opt_defined(short) {
        i = matches.opt_str(short).map(|x| x.parse::<i64>().unwrap());
    };

    i.or_else(|| {
            config.lookup(long).and_then(|v| f(v)).or_else(|| {
                info!("malformed or missing {}, use default {:?}", long, default);
                default
            })
        })
        .expect(&format!("please specify {}", long))
}

fn initial_log(matches: &Matches, config: &toml::Value) {
    let level = get_string_value("L",
                                 "server.log-level",
                                 matches,
                                 config,
                                 Some("info".to_owned()),
                                 |v| v.as_str().map(|s| s.to_owned()));
    util::init_log(logger::get_level_by_string(&level)).unwrap();
}

fn initial_metric(matches: &Matches, config: &toml::Value, node_id: Option<u64>) {
    let host = get_string_value("metric-addr",
                                "metric.addr",
                                matches,
                                config,
                                Some("".to_owned()),
                                |v| v.as_str().map(|s| s.to_owned()));
    let mut prefix = get_string_value("metric-prefix",
                                      "metric.prefix",
                                      matches,
                                      config,
                                      Some("tikv".to_owned()),
                                      |v| v.as_str().map(|s| s.to_owned()));
    if let Some(node_id) = node_id {
        prefix.push_str(&format!(".{}", node_id));
    }

    if !host.is_empty() {
        // We only need a unique UDP bind, so 0.0.0.0:0 is enough.
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let sink = NonblockUdpMetricSink::from(&*host, socket).unwrap();
        let client = StatsdClient::from_sink(&prefix, sink);
        if let Err(r) = metric::set_metric_client(Box::new(client)) {
            error!("{}", r);
        }
    } else {
        let client = StatsdClient::from_sink(&prefix, NopMetricSink);
        if let Err(r) = metric::set_metric_client(Box::new(client)) {
            error!("{}", r);
        }
    }
}

fn get_rocksdb_option(matches: &Matches, config: &toml::Value) -> RocksdbOptions {
    let mut opts = RocksdbOptions::new();
    let mut block_base_opts = BlockBasedOptions::new();
    let block_size = get_integer_value("",
                                       "rocksdb.block-based-table.block-size",
                                       matches,
                                       config,
                                       Some(64 * 1024),
                                       |v| v.as_integer());
    block_base_opts.set_block_size(block_size as u64);
    opts.set_block_based_table_factory(&block_base_opts);

    let tp = get_string_value("",
                              "rocksdb.compression",
                              matches,
                              config,
                              Some("lz4".to_owned()),
                              |v| v.as_str().map(|s| s.to_owned()));
    let compression = util::rocksdb_option::get_compression_by_string(&tp);
    opts.compression(compression);

    let write_buffer_size = get_integer_value("",
                                              "rocksdb.write-buffer-size",
                                              matches,
                                              config,
                                              Some(96 * 1024 * 1024),
                                              |v| v.as_integer());
    opts.set_write_buffer_size(write_buffer_size as u64);

    let max_write_buffer_number = {
        get_integer_value("",
                          "rocksdb.max-write-buffer-number",
                          matches,
                          config,
                          Some(5),
                          |v| v.as_integer())
    };
    opts.set_max_write_buffer_number(max_write_buffer_number as i32);

    let min_write_buffer_number_to_merge = {
        get_integer_value("",
                          "rocksdb.min-write-buffer-number-to-merge",
                          matches,
                          config,
                          Some(2),
                          |v| v.as_integer())
    };
    opts.set_min_write_buffer_number_to_merge(min_write_buffer_number_to_merge as i32);

    let max_background_compactions = get_integer_value("",
                                                       "rocksdb.max-background-compactions",
                                                       matches,
                                                       config,
                                                       Some(3),
                                                       |v| v.as_integer());
    opts.set_max_background_compactions(max_background_compactions as i32);

    let max_bytes_for_level_base = get_integer_value("",
                                                     "rocksdb.max-bytes-for-level-base",
                                                     matches,
                                                     config,
                                                     Some(64 * 1024 * 1024),
                                                     |v| v.as_integer());
    opts.set_max_bytes_for_level_base(max_bytes_for_level_base as u64);

    let target_file_size_base = get_integer_value("",
                                                  "rocksdb.target-file-size-base",
                                                  matches,
                                                  config,
                                                  Some(16 * 1024 * 1024),
                                                  |v| v.as_integer());
    opts.set_target_file_size_base(target_file_size_base as u64);

    let create_if_missing = config.lookup("rocksdb.create-if-missing")
        .unwrap_or(&toml::Value::Boolean(true))
        .as_bool()
        .unwrap_or(true);
    opts.create_if_missing(create_if_missing);

    let level_zero_slowdown_writes_trigger = {
        get_integer_value("",
                          "rocksdb.level0-slowdown-writes-trigger",
                          matches,
                          config,
                          Some(12),
                          |v| v.as_integer())
    };
    opts.set_level_zero_slowdown_writes_trigger(level_zero_slowdown_writes_trigger as i32);

    let level_zero_stop_writes_trigger = get_integer_value("",
                                                           "rocksdb.level0-stop-writes-trigger",
                                                           matches,
                                                           config,
                                                           Some(24),
                                                           |v| v.as_integer());
    opts.set_level_zero_stop_writes_trigger(level_zero_stop_writes_trigger as i32);

    opts
}

fn build_cfg(matches: &Matches, config: &toml::Value, cluster_id: u64, addr: &str) -> Config {
    let mut cfg = Config::new();
    cfg.cluster_id = cluster_id;
    cfg.addr = addr.to_owned();
    cfg.notify_capacity = get_integer_value("",
                                            "server.notify-capacity",
                                            matches,
                                            config,
                                            Some(40960),
                                            |v| v.as_integer()) as usize;
    let capacity = get_integer_value("capacity",
                                     "server.capacity",
                                     matches,
                                     config,
                                     Some(0),
                                     |v| v.as_integer());
    assert!(capacity >= 0);
    if capacity > 0 {
        cfg.store_cfg.capacity = capacity as u64;
    }

    // Set advertise address for outer node and client use.
    // If no advertise listening address set, use the associated listening address.
    cfg.advertise_addr = get_string_value("advertise-addr",
                                          "server.advertise-addr",
                                          matches,
                                          config,
                                          Some(addr.to_owned()),
                                          |v| v.as_str().map(|s| s.to_owned()));
    cfg.store_cfg.notify_capacity =
        get_integer_value("",
                          "raftstore.notify-capacity",
                          matches,
                          config,
                          Some(40960),
                          |v| v.as_integer()) as usize;
    cfg.store_cfg.region_split_size =
        get_integer_value("region-split-size",
                          "raftstore.region-split-size",
                          matches,
                          config,
                          Some(64 * 1024 * 1024),
                          |v| v.as_integer()) as u64;
    cfg.store_cfg.region_max_size =
        get_integer_value("region-max-size",
                          "raftstore.region-max-size",
                          matches,
                          config,
                          Some(80 * 1024 * 1024),
                          |v| v.as_integer()) as u64;
    cfg.store_cfg.region_check_size_diff =
        get_integer_value("region-split-check-diff",
                          "raftstore.region-split-check-diff",
                          matches,
                          config,
                          Some(8 * 1024 * 1024),
                          |v| v.as_integer()) as u64;

    cfg.store_cfg.pd_heartbeat_tick_interval =
        get_integer_value("pd-heartbeat-tick-interval",
                          "raftstore.pd-heartbeat-tick-interval",
                          matches,
                          config,
                          Some(5000),
                          |v| v.as_integer()) as u64;

    cfg.store_cfg.pd_store_heartbeat_tick_interval =
        get_integer_value("pd-store-heartbeat-tick-interval",
                          "raftstore.pd-store-heartbeat-tick-interval",
                          matches,
                          config,
                          Some(10000),
                          |v| v.as_integer()) as u64;

    cfg
}

fn build_raftkv(matches: &Matches,
                config: &toml::Value,
                ch: SendCh,
                pd_client: Arc<RpcClient>,
                cfg: &Config)
                -> (Storage, Arc<RwLock<ServerRaftStoreRouter>>, u64, SnapManager) {
    let trans = Arc::new(RwLock::new(ServerTransport::new(ch)));
    let path = Path::new(&get_store_path(matches, config)).to_path_buf();
    let opts = get_rocksdb_option(matches, config);
    let mut db_path = path.clone();
    db_path.push("db");
    let engine = Arc::new(DB::open(&opts, db_path.to_str().unwrap()).unwrap());

    let mut snap_path = path.clone();
    snap_path.push("snap");
    let snap_path = snap_path.to_str().unwrap().to_owned();
    let snap_mgr = store::new_snap_mgr(snap_path);

    let mut event_loop = store::create_event_loop(&cfg.store_cfg).unwrap();
    let mut node = Node::new(&mut event_loop, cfg, pd_client);
    node.start(event_loop, engine.clone(), trans, snap_mgr.clone()).unwrap();
    let raft_router = node.raft_store_router();
    let node_id = node.id();

    (create_raft_storage(node, engine).unwrap(), raft_router, node_id, snap_mgr)
}

fn get_store_path(matches: &Matches, config: &toml::Value) -> String {
    let path = get_string_value("s",
                                "server.store",
                                matches,
                                config,
                                Some(TEMP_DIR.to_owned()),
                                |v| v.as_str().map(|s| s.to_owned()));
    if path == TEMP_DIR {
        return path;
    }

    let p = Path::new(&path);
    if p.exists() && p.is_file() {
        panic!("{} is not a directory!", path);
    }
    if !p.exists() {
        fs::create_dir_all(p).unwrap();
    }
    let absolute_path = p.canonicalize().unwrap();
    format!("{}", absolute_path.display())
}

fn run_local_server(listener: TcpListener, store: Storage, config: &Config) {
    let mut event_loop = create_event_loop(config).unwrap();
    let router = Arc::new(RwLock::new(MockRaftStoreRouter));
    let snap_mgr = store::new_snap_mgr(TEMP_DIR);
    let mut svr = Server::new(&mut event_loop,
                              listener,
                              store,
                              router,
                              MockStoreAddrResolver,
                              snap_mgr)
        .unwrap();
    svr.run(&mut event_loop).unwrap();
}

fn run_raft_server(listener: TcpListener, matches: &Matches, config: &toml::Value, cfg: &Config) {
    let mut event_loop = create_event_loop(cfg).unwrap();
    let ch = SendCh::new(event_loop.channel());
    let pd_addr = get_string_value("pd",
                                   "raft.pd",
                                   matches,
                                   config,
                                   None,
                                   |v| v.as_str().map(|s| s.to_owned()));
    let pd_client = Arc::new(new_rpc_client(&pd_addr, cfg.cluster_id).unwrap());
    let resolver = PdStoreAddrResolver::new(pd_client.clone()).unwrap();

    let store_path = get_store_path(matches, config);
    let mut lock_path = Path::new(&store_path).to_path_buf();
    lock_path.push("LOCK");
    let f = File::create(lock_path).unwrap();
    if f.try_lock_exclusive().is_err() {
        panic!("lock {} failed, maybe another instance is using this directory.",
               store_path);
    }

    let (store, raft_router, node_id, snap_mgr) =
        build_raftkv(matches, config, ch.clone(), pd_client, cfg);
    info!("tikv server config: {:?}", cfg);
    initial_metric(matches, config, Some(node_id));
    let mut svr = Server::new(&mut event_loop,
                              listener,
                              store,
                              raft_router,
                              resolver,
                              snap_mgr)
        .unwrap();
    svr.run(&mut event_loop).unwrap();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();
    let mut opts = Options::new();
    opts.optopt("A",
                "addr",
                "set listening address",
                "default is 127.0.0.1:20160");
    opts.optopt("",
                "advertise-addr",
                "set advertise listening address for client communication",
                "127.0.0.1:20160, if not set, use addr instead.");
    opts.optopt("L",
                "log",
                "set log level",
                "log level: trace, debug, info, warn, error, off");
    opts.optflag("h", "help", "print this help menu");
    opts.optopt("C", "config", "set configuration file", "file path");
    opts.optopt("s",
                "store",
                "set the path to store directory",
                "/tmp/tikv/store");
    opts.optopt("",
                "capacity",
                "set the store capacity",
                "default: 0 (unlimited)");
    opts.optopt("S",
                "dsn",
                "set which dsn to use, warning: default is rocksdb without persistent",
                "dsn: rocksdb, raftkv");
    opts.optopt("I", "cluster-id", "set cluster id", "must greater than 0.");
    opts.optopt("", "pd", "set pd address", "host:port");
    opts.optopt("", "metric-addr", "set statsd server address", "host:port");
    opts.optopt("",
                "metric-prefix",
                "set metric prefix",
                "metric prefix: tikv");
    opts.optopt("",
                "region-split-size",
                "set region split size",
                "default: 64 MB");
    opts.optopt("",
                "region-max-size",
                "set region max size",
                "default: 80 MB");
    opts.optopt("",
                "region-split-check-diff",
                "set region split check diff",
                "default: 8 MB");
    opts.optopt("",
                "pd-heartbeat-tick-interval",
                "set region heartbeat tick interval",
                "default 5000 (ms)");
    opts.optopt("",
                "pd-store-heartbeat-tick-interval",
                "set region store heartbeat tick interval",
                "default 5000 (ms)");

    let matches = opts.parse(&args[1..]).expect("opts parse failed");
    if matches.opt_present("h") {
        print_usage(&program, opts);
        return;
    }

    let config = match matches.opt_str("C") {
        Some(path) => {
            let mut config_file = fs::File::open(&path).expect("config open filed");
            let mut s = String::new();
            config_file.read_to_string(&mut s).expect("config read filed");
            toml::Value::Table(toml::Parser::new(&s).parse().expect("malformed config file"))
        }
        // Empty value, lookup() always return `None`.
        None => toml::Value::Integer(0),
    };

    initial_log(&matches, &config);
    let addr = get_string_value("A",
                                "server.addr",
                                &matches,
                                &config,
                                Some(DEFAULT_LISTENING_ADDR.to_owned()),
                                |v| v.as_str().map(|s| s.to_owned()));
    info!("Start listening on {}...", addr);
    let listener = bind(&addr).unwrap();
    let dsn_name = get_string_value("S",
                                    "server.dsn",
                                    &matches,
                                    &config,
                                    Some(ROCKSDB_DSN.to_owned()),
                                    |v| v.as_str().map(|s| s.to_owned()));
    panic_hook::set_exit_hook();
    let id = get_string_value("I",
                              "raft.cluster-id",
                              &matches,
                              &config,
                              None,
                              |v| v.as_integer().map(|i| i.to_string()));
    let cluster_id = u64::from_str_radix(&id, 10).expect("invalid cluster id");
    let cfg = build_cfg(&matches,
                        &config,
                        cluster_id,
                        &format!("{}", listener.local_addr().unwrap()));
    match dsn_name.as_ref() {
        ROCKSDB_DSN => {
            initial_metric(&matches, &config, None);
            let path = get_store_path(&matches, &config);
            let store = Storage::new(Dsn::RocksDBPath(&path)).unwrap();
            run_local_server(listener, store, &cfg);
        }
        RAFTKV_DSN => {
            run_raft_server(listener, &matches, &config, &cfg);
        }
        n => panic!("unrecognized dns name: {}", n),
    };
}
