extern crate raio;

use std::thread;
use std::time::Duration;
use std::io::prelude::*;
use std::net::{TcpListener};
use raio::{Executor, SingleThreadedExecutor, AsyncTcpListener, AsyncTcpStream, EventControl};

fn main() {
    let mut executor = SingleThreadedExecutor::new("executor-0");
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

                    let mut data:Vec<u8> = Vec::new();
                    for i in 1..1024 * 1024{
                        if i%26 == 0 {
                            data.push('\n' as u8);
                        }
                        data.push((65 + i%26) as u8);
                    }
                    data.push('\n' as u8);

                    s.write_async( &executor, data);
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


    executor.execute(|| {
        println!("hi0 on thread {:?}", thread::current());
    });

    executor.execute(|| {
        println!("hi1 on thread {:?}", thread::current());
    });
    
    executor.execute(|| {
        println!("hi2 on thread {:?}", thread::current());
    });

    executor.schedule( || {
        println!("timer  thread {:?}", thread::current());
        EventControl::KEEP
    }, Duration::new(2, 0));

    executor.join();
}
