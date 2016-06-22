extern crate nio2;

use std::io::prelude::*;
use std::net::{TcpListener};
use std::time::Duration;
use std::thread;
use nio2::{Executor, SingleThreadedExecutor, AsyncTcpListener, AsyncTcpStream, EventControl};

fn main() {
    let executor = SingleThreadedExecutor::new("foo");
    let executor1 = SingleThreadedExecutor::new("foo");
    let listener = TcpListener::bind("127.0.0.1:5432").unwrap();


    listener.accept_async( &executor, move | list | {
        println!("Accepting");
        let (stream, addr) = list.accept().unwrap();

        stream.read_async( &executor1, move | s | {
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

    thread::sleep(Duration::new(5, 0));
}
