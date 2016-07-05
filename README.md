# raio
Rust multithreaded async IO [![Build Status](https://travis-ci.org/chsitter/raio.svg?branch=master)](https://travis-ci.org/chsitter/raio)

The [`raio`](http://chsitter.github.io/raio/) crate is designed to provide an easy API to execute and schedule events on specific threads as well as registering asynchronous events on file descriptors

The aim here is to create an abstraction that allows for different eventloop implementations to be used for different threads, hence the API will probably change a bit. 
- currently only supports kqueue
- async write not done yet
- doesn't yet have a "context" object that could be used as an argument to a fn that gets registered 
- it's all still a bit hacky and messy

## Usage
``` rust
extern crate raio;

use std::thread;
use std::time::Duration;
use std::io::prelude::*;
use std::net::{TcpListener};
use raio::{Executor, SingleThreadedExecutor, AsyncTcpListener, AsyncTcpStream, EventControl};

fn main() {
    let mut executor = SingleThreadedExecutor::new("exec-0");
    
    //asynchronously accepting socket connections
    let listener = TcpListener::bind("127.0.0.1:15000").unwrap();

    listener.accept_async( &executor, | list | {
      let (stream, addr) = list.accept().unwrap();
      EventControl::KEEP
    }
    
    //asynchronous read
    let (stream, addr) = listener.accept().unwrap();
    stream.read_async( &executor, | s | {
      let mut buf = [0; 1024];
      match s.read(&mut buf) {
        OK(r) if r < 0 => {
          EventControl::DELETE
        },
        OK(r) => {
          EventControl::KEEP
        },
        Err(e) => {
          EventControl::DELETE
        }
      }
    }
    
    //execute
    executor.execute( || {
      println!("executing on thread {:?}", thread::current());
    }
    
    //timers (return KEEP for recurring, DELETE for oneshot)
    executor.schedule( || {
      println!("timer on thread {:?}", thread::current());
      EventControl::KEEP    //keep firing
    }, Duration::new(2,0));
    
    
    executor.join();  //wait forever
}
```

## Contributors
* [Christoph Sitter](https://github.com/chsitter/)

## License
Copyright © 2015 Christoph Sitter

Distributed under the [Apache License Version 2.0](LICENSE).
