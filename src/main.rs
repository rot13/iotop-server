/*
This is free and unencumbered software released into the public domain.

Anyone is free to copy, modify, publish, use, compile, sell, or
distribute this software, either in source code form or as a compiled
binary, for any purpose, commercial or non-commercial, and by any
means.

In jurisdictions that recognize copyright laws, the author or authors
of this software dedicate any and all copyright interest in the
software to the public domain. We make this dedication for the benefit
of the public at large and to the detriment of our heirs and
successors. We intend this dedication to be an overt act of
relinquishment in perpetuity of all present and future rights to this
software under copyright law.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR
OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.

For more information, please refer to <http://unlicense.org/>
*/

use std::process::Command;
use std::process::Stdio;
use std::io::BufReader;
use std::io::BufRead;
use std::env;

extern crate getopts;
use getopts::Options;

extern crate regex;
use regex::Regex;

extern crate time;
use time::get_time;

extern crate rustc_serialize;
use rustc_serialize::json;

use std::thread;
use std::sync::{Arc, Mutex, RwLock, Condvar};

extern crate websocket;
use websocket::{Server, Message, Sender};
use websocket::header::WebSocketProtocol;

#[derive(RustcEncodable)]
struct IOStatLine {
  seq: u32,
  time: i64,
  tid: u32,
  prio: String,
  user: String,
  disk_read_kb: f32,
  disk_write_kb: f32,
  swapin_percent: f32,
  io_percent: f32,
  command: String
}

fn server(addr: &str, iostat_lines_arc: Arc<(RwLock<Vec<IOStatLine>>, Mutex<()>, Condvar)>) {
  let server = Server::bind(addr).unwrap();

  for connection in server {
    // Spawn a new thread for each connection.
    let iostat_lines_arc = iostat_lines_arc.clone();
    let _ = thread::Builder::new().name("iotop srv conn".to_string()).spawn(move || {
      let request = connection.unwrap().read_request().unwrap(); // Get the request
      let headers = request.headers.clone(); // Keep the headers so we can check them

      request.validate().unwrap(); // Validate the request

      let mut response = request.accept(); // Form a response

      if let Some(&WebSocketProtocol(ref protocols)) = headers.get() {
        if protocols.contains(&("rust-websocket".to_string())) {
          // We have a protocol we want to use
          response.headers.set(WebSocketProtocol(vec!["rust-websocket".to_string()]));
        }
      }

      let mut client = response.send().unwrap(); // Send the response

      let ip = client.get_mut_sender()
        .get_mut()
        .peer_addr()
        .unwrap();

      println!("Connection from {}", ip);

      #[derive(RustcEncodable)]
      struct WelcomeMsg { server_time: i64 };

      let msg = WelcomeMsg { server_time: get_time().sec };
      if client.send_message(Message::Text(json::encode(&msg).unwrap())).is_err() {
        println!("websocket error from {}", ip);
        return;
      }

      let mut last_sent = 0;
      let messages: Vec<(u32,Message)> = {
        let &(ref iostat_lines_lock, _, _) = &*iostat_lines_arc;
        let iostat_lines = iostat_lines_lock.read().unwrap();
        iostat_lines.iter().map(
          |line| (line.seq, Message::Text(json::encode(line).unwrap()))
        ).collect()
      };
      for message in messages {
        if client.send_message(message.1).is_err() {
          println!("websocket error from {}", ip);
          return;
        }
        last_sent = message.0;
      }

      loop {
        let messages: Vec<(u32,Message)> = {
          let &(ref iostat_lines_lock, ref iostat_lines_mutex, ref iostat_lines_cvar) = &*iostat_lines_arc;
          let _ = iostat_lines_cvar.wait(iostat_lines_mutex.lock().unwrap()).unwrap();

          let iostat_lines = iostat_lines_lock.read().unwrap();

          let old_last_sent = last_sent;
          let it: Vec<&IOStatLine> = iostat_lines.iter().rev().take_while(
            |l| l.seq > old_last_sent
          ).collect();

          it.iter().rev().map(
            |line| (line.seq, Message::Text(json::encode(line).unwrap()))
          ).collect()
        };
        for message in messages {
          if client.send_message(message.1).is_err() {
            println!("websocket error from {}", ip);
            return;
          }
          last_sent = message.0;
        }
      }

    });
  }
}

fn print_usage(program: &str, opts: Options) {
  let brief = format!("Usage: {} [options]", program);
  print!("{}", opts.usage(&brief));
}

fn main() {

  let args: Vec<String> = env::args().collect();
  let program = args[0].clone();

  let mut opts = Options::new();
  opts.optflag("h", "help", "print this help menu");
  opts.optopt("l", "listen", "set listening address and port", "HOST:PORT");
  opts.optopt("p", "path", "set iotop path", "path-to-iotop");
  opts.optopt("i", "interval", "set time interval in seconds", "sec");

  let matches = match opts.parse(&args[1..]) {
    Ok(m) => { m }
    Err(f) => { panic!(f.to_string()) }
  };

  if matches.opt_present("h") {
    print_usage(&program, opts);
    return;
  }

  let addr = match matches.opt_str("l") {
    None => { "0.0.0.0:9093".to_string() }
    Some(s) => { s }
  };

  let iotop_path = match matches.opt_str("p") {
    None => { "iotop".to_string() }
    Some(s) => { s }
  };

  let interval = match matches.opt_str("i") {
    None => { 900 }
    Some(s) => { s.trim().parse().unwrap_or_else(|e| panic!("{}", e)) }
  };

  let handle = Command::new(iotop_path).
    arg("-bokqqq").
    stdout(Stdio::piped()).
    spawn().
    unwrap_or_else(|e| {
      panic!("Failed to execute process: {}", e)
    }
  );
  let mut reader = BufReader::with_capacity(128, handle.stdout.expect("Failed to get stdout"));

  let iostat_lines_arc: Arc<(RwLock<Vec<IOStatLine>>, Mutex<()>, Condvar)> = 
    Arc::new((RwLock::new(Vec::new()), Mutex::new(()), Condvar::new()));
  {
    let iostat_lines_arc = iostat_lines_arc.clone();
    let _ = thread::Builder::new().name("iotop srv".to_string()).spawn(move || {
      server(&addr, iostat_lines_arc);
    });
  }

  let re = match Regex::new(
    r"^\s*(?P<tid>[0-9]+)\s+(?P<prio>.*?)\s+(?P<user>.*?)\s+(?P<disk_read_kb>[0-9.]+) K/s\s+(?P<disk_write_kb>[0-9.]+) K/s\s+(?P<swapin_percent>[0-9.]+) %\s+(?P<io_percent>[0-9.]+) % (?P<command>.*)\n$"
  ) {
    Ok(re) => re,
    Err(err) => panic!("{}", err),
  };

  let mut seq: u32 = 0;
  loop {
    let input_tmp: &mut Vec<u8> = &mut Vec::new();
    reader.read_until(10, input_tmp).ok().expect("Failed to read line");
    let mut input_tmp = String::from_utf8_lossy(input_tmp);
    let input = input_tmp.to_mut();
    let fields = match re.captures(input) {
      Some(v) => v,
      None => panic!("Match failed, line:\n{}", input)
    };
    let time_sec = get_time().sec;
    {
      let &(ref iostat_lines_lock, _, ref iostat_lines_cvar) = &*iostat_lines_arc;
      let mut iostat_lines = iostat_lines_lock.write().unwrap();
      while !iostat_lines.is_empty() && iostat_lines.first().unwrap().time < time_sec - interval {
        iostat_lines.remove(0);
      }
      iostat_lines.push(
        IOStatLine {
          seq: seq,
          time: time_sec,
          tid: fields.name("tid").unwrap().parse::<u32>().unwrap(),
          prio: fields.name("prio").unwrap().to_string(),
          user: fields.name("user").unwrap().to_string(),
          disk_read_kb: fields.name("disk_read_kb").unwrap().parse::<f32>().unwrap(),
          disk_write_kb: fields.name("disk_write_kb").unwrap().parse::<f32>().unwrap(),
          swapin_percent: fields.name("swapin_percent").unwrap().parse::<f32>().unwrap(),
          io_percent: fields.name("io_percent").unwrap().parse::<f32>().unwrap(),
          command: fields.name("command").unwrap().to_string()
        }
      );
      iostat_lines_cvar.notify_all();
    }
    seq += 1;
  }
}
