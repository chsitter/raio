extern crate raio;

use std::io::prelude::*;
use std::net::{TcpListener};
use std::thread;
use raio::{Executor, SingleThreadedExecutor, AsyncTcpListener, AsyncTcpStream, EventControl};

fn main() {
    let mut executor = SingleThreadedExecutor::new("foo");
    let listener = TcpListener::bind("127.0.0.1:5432").unwrap();

    listener.accept_async( &executor, | list | {
        println!("Accepting");
        let (stream, addr) = list.accept().unwrap();

        stream.read_async( &executor, | s | {
            println!("reading");
            let mut buf = [0; 1024];

            match s.read(&mut buf) {
                Ok(r) if r <= 0     => {
                    println!("ERROR: read {} bytes", r);
                    EventControl::DELETE
                },
                Ok(r)               => {
                    println!("read {} bytes", r);
                    EventControl::KEEP
                },
                Err(e) => {
                    println!("error {}", e);
                    EventControl::DELETE
                }
            }
        });

        println!("accepted {:?}", addr);
        EventControl::KEEP
    });

    executor.join();
}
