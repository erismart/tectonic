/// Server should handle requests similar to Redis
/// 
/// List of commands:
/// -------------------------------------------

static HELP_STR : &str = "PING, INFO, USE [db], CREATE [db],
ADD [ts],[seq],[is_trade],[is_bid],[price],[size];
BULKADD ...; DDAKLUB
FLUSH, FLUSHALL, GETALL, GET [count], CLEAR
";

use byteorder::{BigEndian, WriteBytesExt, /*ReadBytesExt*/};

use std::error::Error;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::str;
use std::fs;

use dtf;

/// name: *should* be the filename
/// in_memory: are the updates read into memory?
/// size: true number of items
/// v: vector of updates
///
///
/// When client connects, the following happens:
///
/// 1. server creates a State
/// 2. initialize 'default' data store
/// 3. reads filenames under dtf_folder
/// 4. loads metadata but not updates
/// 5. client can retrieve server status using INFO command
///
/// When client adds some updates using ADD or BULKADD,
/// size increments and updates are added to memory
/// finally, call FLUSH to commit to disk the current store or FLUSHALL to commit all available stores.
/// the client can free the updates from memory using CLEAR or CLEARALL
///

#[derive(Debug)]
struct Store {
    name: String,
    folder: String,
    in_memory: bool,
    size: u64,
    v: Vec<dtf::Update>
}

impl Store {
    /// Push a new `Update` into the vec
    fn add(&mut self, new_vec : dtf::Update) {
        self.size = self.size + 1;
        self.v.push(new_vec);
    }

    /// write items stored in memory into file
    /// If file exists, use append which only appends a filtered set of updates whose timestamp is larger than the old timestamp
    /// If file doesn't exists, simply encode.
    /// 
    /// TODO: Need to figure out how to specify symbol (and exchange name).
    fn flush(&self) -> Option<bool> {
        let fname = format!("{}/{}.dtf", self.folder, self.name);
        create_dir_if_not_exist(&self.folder);
        if Path::new(&fname).exists() {
            dtf::append(&fname, &self.v);
            return Some(true);
        } else {
            dtf::encode(&fname, &self.name /*XXX*/, &self.v);
        }
        Some(true)
    }

    /// load items from dtf file
    fn load(&mut self) {
        let fname = format!("{}/{}.dtf", self.folder, self.name);
        if Path::new(&fname).exists() && !self.in_memory {
            self.v = dtf::decode(&fname);
            self.size = self.v.len() as u64;
            self.in_memory = true;
        }
    }

    /// load size from file
    fn load_size_from_file(&mut self) {
        let header_size = dtf::get_size(&format!("{}/{}.dtf", self.folder, self.name));
        self.size = header_size;
    }

    /// clear the vector. toggle in_memory. update size
    fn clear(&mut self) {
        self.v.clear();
        self.in_memory = false;
        self.load_size_from_file();
    }
}


/// Each client gets its own State
struct State {
    is_adding: bool,
    store: HashMap<String, Store>,
    current_store_name: String,
    settings: Settings
}
impl State {
    fn insert(&mut self, up: dtf::Update, store_name : &str) {
        let store = self.store.get_mut(store_name).expect("KEY IS NOT IN HASHMAP");
        store.add(up);
    }

    fn add(&mut self, up: dtf::Update) {
        let current_store = self.store.get_mut(&self.current_store_name).expect("KEY IS NOT IN HASHMAP");
        current_store.add(up);
    }

    fn autoflush(&mut self) {
        let current_store = self.store.get_mut(&self.current_store_name).expect("KEY IS NOT IN HASHMAP");
        if self.settings.autoflush && current_store.size % self.settings.flush_interval as u64 == 0 {
            println!("(AUTO) FLUSHING!");
            current_store.flush();
            current_store.load_size_from_file();
        }
    }

    fn get(&self, count : i32) -> Option<Vec<u8>> {
        let mut bytes : Vec<u8> = Vec::new();
        let current_store = self.store.get(&self.current_store_name).unwrap();
        if (current_store.size as i32) < count || current_store.size == 0 {
            None
        } else {
            match count {
                -1 => {
                    dtf::write_batches(&mut bytes, &current_store.v);
                },
                _ => {
                    dtf::write_batches(&mut bytes, &current_store.v[..count as usize]);
                }
            }
            Some(bytes)
        }
    }

}

/// Parses a line that looks like 
/// 
/// 1505177459.658, 139010, t, t, 0.0703629, 7.65064249;
/// 
/// into an `Update` struct.
/// 
fn parse_line(string : &str) -> Option<dtf::Update> {
    let mut u = dtf::Update { ts : 0, seq : 0, is_bid : false, is_trade : false, price : -0.1, size : -0.1 };
    let mut buf : String = String::new();
    let mut count = 0;
    let mut most_current_bool = false;

    for ch in string.chars() {
        if ch == '.' && count == 0 {
            continue;
        } else if ch == '.' && count != 0 {
            buf.push(ch);
        } else if ch.is_digit(10) {
            buf.push(ch);
        } else if ch == 't' || ch == 'f' {
            most_current_bool = ch == 't';
        } else if ch == ',' || ch == ';' {
            match count {
                0 => { u.ts       = match buf.parse::<u64>() {Ok(ts) => ts, Err(_) => return None}},
                1 => { u.seq      = match buf.parse::<u32>() {Ok(seq) => seq, Err(_) => return None}},
                2 => { u.is_trade = most_current_bool; },
                3 => { u.is_bid   = most_current_bool; },
                4 => { u.price    = match buf.parse::<f32>() {Ok(price) => price, Err(_) => return None} },
                5 => { u.size     = match buf.parse::<f32>() {Ok(size) => size, Err(_) => return None}},
                _ => panic!("IMPOSSIBLE")
            }
            count += 1;
            buf.clear();
        }
    }

    if u.price < 0. || u.size < 0. {
        None
    } else {
        Some(u)
    }
}

fn gen_response(string : &str, state: &mut State) -> (Option<String>, Option<Vec<u8>>, Option<String>) {
    match string {
        "" => (Some("".to_owned()), None, None),
        "PING" => (Some("PONG.\n".to_owned()), None, None),
        "HELP" => (Some(HELP_STR.to_owned()), None, None),
        "INFO" => {
            let info_vec : Vec<String> = state.store.values().map(|store| {
                format!(r#"{{"name": "{}", "in_memory": {}, "count": {}}}"#, store.name, store.in_memory, store.size)
            }).collect();

            (Some(format!("[{}]\n", info_vec.join(", "))), None, None)
        },
        "BULKADD" => {
            state.is_adding = true;
            (Some("".to_owned()), None, None)
        },
        "DDAKLUB" => {
            state.is_adding = false;
            (Some("1\n".to_owned()), None, None)
        },
        "GET ALL AS JSON" => {
            let current_store = state.store.get(&state.current_store_name).unwrap();
            let json = dtf::update_vec_to_json(&current_store.v);
            let json = format!("[{}]\n", json);
            (Some(json), None, None)
        },
        "GET ALL" => {
            match state.get(-1) {
                Some(bytes) => (None, Some(bytes), None),
                None => (None, None, Some("Failed to GET ALL.".to_owned()))
            }
        },
        "CLEAR" => {
            let current_store = state.store.get_mut(&state.current_store_name).expect("KEY IS NOT IN HASHMAP");
            current_store.clear();
            (Some("1\n".to_owned()), None, None)
        },
        "CLEAR ALL" => {
            for store in state.store.values_mut() {
                store.clear();
            }
            (Some("1\n".to_owned()), None, None)
        },
        "FLUSH" => {
            let current_store = state.store.get_mut(&state.current_store_name).expect("KEY IS NOT IN HASHMAP");
            current_store.flush();
            (Some("1\n".to_owned()), None, None)
        },
        "FLUSH ALL" => {
            for store in state.store.values() {
                store.flush();
            }
            (Some("1\n".to_owned()), None, None)
        },
        _ => {
            // bulkadd and add
            if state.is_adding {
                let parsed = parse_line(string);
                match parsed {
                    Some(up) => {
                        state.add(up);
                        state.autoflush();
                    }
                    None => return (None, None, Some("Unable to parse line in BULKALL".to_owned()))
                }
                (Some("".to_owned()), None, None)
            } else

            if string.starts_with("ADD ") {
                if string.contains(" INTO ") {
                    let into_indices : Vec<_> = string.match_indices(" INTO ").collect();
                    let (index, _) = into_indices[0];
                    let dbname = &string[(index+6)..];
                    let data_string : &str = &string[3..(index-2)];
                    match parse_line(&data_string) {
                        Some(up) => {
                            state.insert(up, dbname);
                            state.autoflush();
                            (Some("1\n".to_owned()), None, None)
                        },
                        None => return (None, None, Some("Parse ADD INTO".to_owned()))
                    }
                } else {
                    let data_string : &str = &string[3..];
                    match parse_line(&data_string) {
                        Some(up) => {
                            state.add(up);
                            state.autoflush();
                            (Some("1\n".to_owned()), None, None)
                        }
                        None => return (None, None, Some("Parse ADD".to_owned()))
                    }
                }
            } else 

            // db commands
            if string.starts_with("CREATE ") {
                let dbname : &str = &string[7..];
                state.store.insert(dbname.to_owned(), Store {
                    name: dbname.to_owned(),
                    v: Vec::new(),
                    size: 0,
                    in_memory: false,
                    folder: state.settings.dtf_folder.clone()
                });
                (Some(format!("Created DB `{}`.\n", &dbname)), None, None)
            } else

            if string.starts_with("USE ") {
                let dbname : &str = &string[4..];
                if state.store.contains_key(dbname) {
                    state.current_store_name = dbname.to_owned();
                    let current_store = state.store.get_mut(&state.current_store_name).unwrap();
                    current_store.load();
                    (Some(format!("SWITCHED TO DB `{}`.\n", &dbname)), None, None)
                } else {
                    (None, None, Some(format!("State does not contain {}", dbname)))
                }
            } else

            // get
            if string.starts_with("GET ") {
                let num : &str = &string[4..];
                let count : Vec<&str> = num.split(" ").collect();
                let count = count[0].parse::<i32>().unwrap();

                if string.contains("AS JSON") {
                    let current_store = state.store.get(&state.current_store_name).unwrap();

                    if (current_store.size as i32) <= count || current_store.size == 0 {
                        (None, None, Some("Requested too many".to_owned()))
                    } else {
                        let json = dtf::update_vec_to_json(&current_store.v[..count as usize]);
                        let json = format!("[{}]\n", json);
                        (Some(json), None, None)
                    }
                } else {
                    match state.get(count) {
                        Some(bytes) => (None, Some(bytes), None),
                        None => (None, None, Some(format!("Failed to get {}.", count)))
                    }
                }
            }

            else {
                (None, None, Some("Unsupported command.".to_owned()))
            }
        }
    }
}

fn create_dir_if_not_exist(dtf_folder : &str) {
    if !Path::new(dtf_folder).exists() {
        fs::create_dir(dtf_folder).unwrap();
    }
}

/// Iterate through the dtf files in the folder and load some metadata into memory.
/// Create corresponding Store objects in State.
fn init_dbs(dtf_folder : &str, state: &mut State) {
    for dtf_file in fs::read_dir(&dtf_folder).unwrap() {
        let dtf_file = dtf_file.unwrap();
        let fname_os = dtf_file.file_name();
        let fname = fname_os.to_str().unwrap();
        if fname.ends_with(".dtf") {
            let name = Path::new(&fname_os).file_stem().unwrap().to_str().unwrap();
            let header_size = dtf::get_size(&format!("{}/{}", dtf_folder, fname));
            state.store.insert(name.to_owned(), Store {
                folder: dtf_folder.to_owned(),
                name: name.to_owned(),
                v: Vec::new(),
                size: header_size,
                in_memory: false
            });
        }
    }
}

fn init_state(settings: &Settings, dtf_folder: &str) -> State {
    let mut state = State {
        current_store_name: "default".to_owned(),
        is_adding: false,
        store: HashMap::new(),
        settings: settings.clone()
    };
    let default_file = format!("{}/default.dtf", settings.dtf_folder);
    let default_in_memory = !Path::new(&default_file).exists();
    state.store.insert("default".to_owned(), Store {
        name: "default".to_owned(),
        v: Vec::new(),
        size: 0,
        in_memory: default_in_memory,
        folder: dtf_folder.to_owned(),
    });
    state
}

fn handle_client(mut stream: TcpStream, settings : &Settings) {
    let dtf_folder = &settings.dtf_folder;
    create_dir_if_not_exist(&dtf_folder);
    let mut state = init_state(&settings, &dtf_folder);
    init_dbs(&dtf_folder, &mut state);

    let mut buf = [0; 2048];
    loop {
        let bytes_read = stream.read(&mut buf).unwrap();
        if bytes_read == 0 { break }
        let req = str::from_utf8(&buf[..(bytes_read-1)]).unwrap();

        let resp = gen_response(&req, &mut state);
        match resp {
            (Some(str_resp), None, _) => {
                stream.write_u8(0x1).unwrap();
                stream.write_u64::<BigEndian>(str_resp.len() as u64).unwrap();
                stream.write(str_resp.as_bytes()).unwrap()
            },
            (None, Some(bytes), _) => {
                stream.write_u8(0x1).unwrap();
                stream.write(&bytes).unwrap()
            },
            (None, None, Some(msg)) => {
                stream.write_u8(0x0).unwrap();
                let ret = format!("ERR: {}\n", msg);
                stream.write_u64::<BigEndian>(ret.len() as u64).unwrap();
                stream.write(ret.as_bytes()).unwrap()
            },
            _ => panic!("IMPOSSIBLE")
        };
    }
}

#[derive(Clone)]
pub struct Settings {
    pub autoflush: bool,
    pub dtf_folder: String,
    pub flush_interval: u32,
}

pub fn run_server(host : &str, port : &str, verbosity : u64, settings: &Settings) {
    let addr = format!("{}:{}", host, port);

    if verbosity > 1 {
        println!("[DEBUG] Trying to bind to addr: {}", addr);
        if settings.autoflush {
            println!("[DEBUG] Autoflush is true: every {} inserts.", settings.flush_interval);
        }
    }

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => panic!(format!("{:?}", e.description()))
    };

    if verbosity > 0 {
        println!("Listening on addr: {}", addr);
    }

    for stream in listener.incoming() {
        let stream = stream.unwrap();
        let settings_copy = settings.clone();
        thread::spawn(move || {
            handle_client(stream, &settings_copy);
        });
    }
}

#[test]
fn should_parse_string_not_okay() {
    let string = "1505177459.658, 139010,,, f, t, 0.0703629, 7.65064249;";
    assert!(parse_line(&string).is_none());
    let string = "150517;";
    assert!(parse_line(&string).is_none());
    let string = "something;";
    assert!(parse_line(&string).is_none());
}

#[test]
fn should_parse_string_okay() {
    let string = "1505177459.658, 139010, f, t, 0.0703629, 7.65064249;";
    let target = dtf::Update {
        ts: 1505177459658,
        seq: 139010,
        is_trade: false,
        is_bid: true,
        price: 0.0703629,
        size: 7.65064249
    };
    assert_eq!(target, parse_line(&string).unwrap());


    let string1 = "1505177459.650, 139010, t, f, 0.0703620, 7.65064240;";
    let target1 = dtf::Update {
        ts: 1505177459650,
        seq: 139010,
        is_trade: true,
        is_bid: false,
        price: 0.0703620,
        size: 7.65064240
    };
    assert_eq!(target1, parse_line(&string1).unwrap());
}