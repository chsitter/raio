extern crate libc;

use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::Duration;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::thread;
use std::os::unix::io::IntoRawFd;
use std::os::unix::io::FromRawFd;

pub trait AsyncTcpListener {
    fn accept_async<'a, F, T: Executor>(&self, event_loop: &'a T, accept_cb: F) where F: Fn(&mut TcpListener) -> EventControl + Send + 'a;
}

pub trait AsyncTcpStream {
    fn read_async<'a, F, T: Executor>(&self, event_loop: &'a T, read_cb: F) where F: Fn(&mut TcpStream) -> EventControl + Send + 'a;
}

pub enum EventControl {
    KEEP,
    DELETE
}

pub struct Future;

pub trait Executor : Drop {
    fn new(name: &str) -> Self;
    fn execute<F: Fn() + Send + 'static>(&self, callback: F);
    fn schedule<F: Fn() + Send + 'static>(&self, callback: F, delay: Duration) -> Future;

    fn shutdown(&self);
    fn join(&mut self);

    fn accept<F: Fn(&mut TcpListener) -> EventControl + Send>(&self, listener: TcpListener, callback: F);
    fn read<F: Fn(&mut TcpStream) -> EventControl + Send>(&self, stream: TcpStream, callback: F);

    fn notify(&self);
}

impl AsyncTcpListener for TcpListener {
    fn accept_async<'a, F, T: Executor>(&self, event_loop: &'a T, accept_cb: F) where F: Fn(&mut TcpListener) -> EventControl + Send + 'a {
        self.set_nonblocking(true).unwrap();

        event_loop.accept(self.try_clone().unwrap(), accept_cb);
    }
}

impl AsyncTcpStream for TcpStream {
    fn read_async<'a, F, T: Executor>(&self, event_loop: &'a T, read_cb: F) where F: Fn(&mut TcpStream) -> EventControl + Send + 'a {
        self.set_nonblocking(true).unwrap();

        event_loop.read(self.try_clone().unwrap(), read_cb);
    }
}

enum ThreadMessage {
    Shutdown,
    Execute {
        callback: Box<Fn() + Send>
    },
    Schedule {
        delay: Duration,
        callback: Box<Fn() + Send>
    },
    AddAcceptEvent {
        fd: i32,
        callback: Box<Fn(&mut TcpListener) -> EventControl + Send>
    },
    AddReadEvent {
        fd: i32,
        callback: Box<Fn(&mut TcpStream) -> EventControl + Send>
    }
}

pub struct SingleThreadedExecutor {
    join_handle: Mutex<Option<JoinHandle<()>>>,  //TODO: make RefCell to not need to have mut executors?
    kqid: i32,
    sender: Mutex<Sender<ThreadMessage>>
}

impl Executor for SingleThreadedExecutor {
    fn new(name: &str) -> Self {

        let (tx, rx): (Sender<ThreadMessage>, Receiver<ThreadMessage>) = channel();
        let kq;
        unsafe {
             kq = unsafe { libc::kqueue() as i32 };
        }

        let x = SingleThreadedExecutor {
            sender: Mutex::new(tx),
            kqid: kq,
            join_handle: Mutex::new(Some(thread::Builder::new().name(name.to_string()).spawn( move || {
                executor_loop(kq, rx);
            }).unwrap()))
        };

        thread::sleep(Duration::new(2, 0)); //TODO: use a condvar to wait for everything to be set up
        x
    }

    fn execute<F: Fn() + Send + 'static>(&self, callback: F) {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::Execute {
            callback: Box::new(callback)
        }).unwrap();

        self.notify();
    }

    fn schedule<F: Fn() + Send + 'static>(&self, callback: F, delay: Duration) -> Future {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::Schedule {
            delay: delay,
            callback: Box::new(callback)
        }).unwrap();

        self.notify();

        Future
    }

    fn accept<F: Fn(&mut TcpListener) -> EventControl + Send + 'static>(&self, listener: TcpListener, callback: F) {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::AddAcceptEvent {
            fd: listener.into_raw_fd(),
            callback: Box::new(callback)
        }).unwrap();
        
        self.notify();
    }

    fn read<F: Fn(&mut TcpStream) -> EventControl + Send + 'static>(&self, stream: TcpStream, callback: F) {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::AddReadEvent {
            fd: stream.into_raw_fd(),
            callback: Box::new(callback)
        }).unwrap();

        self.notify();
    }

    fn shutdown(&self) {
        let s = self.sender.lock().unwrap();
        match s.send(ThreadMessage::Shutdown) {
            Ok(()) => {},
            Err(e) => println!("Error occurred!! {}", e)
        }
        
        self.notify();
    }

    fn notify(&self) {
        let ev = libc::kevent {
            ident: 0,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut()
        };

        unsafe {
            libc::kevent(self.kqid, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
        }
    }

    fn join(&mut self) {
        let mut handle = self.join_handle.lock().unwrap();
        if let Some(x) = handle.take() {
            x.join().unwrap();
        }
    }
}

impl Drop for SingleThreadedExecutor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum CallbackType {
    ACCEPT(Box<Fn(&mut TcpListener) -> EventControl>),
    READ(Box<Fn(&mut TcpStream) -> EventControl>)
}

fn executor_loop(kq: i32, receiver: Receiver<ThreadMessage>) {
    let mut nev;

    let num_fds: usize = unsafe {
                let mut rlim: libc::rlimit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim as *mut libc::rlimit );
                rlim.rlim_cur   //TODO: should we be using rlim_max?
    } as usize;

    let mut readevents: Vec<Option<(CallbackType)>> = Vec::with_capacity(num_fds);  //TODO: This should probably be in local storage
    for i in 0..num_fds {
        readevents.push(None);
    }

    let mut timer: Option<Box<Fn()>> = None;

    let mut ev_list: [libc::kevent; 32] = [ libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: std::ptr::null_mut() };32];


    let user_ev = libc::kevent {
        ident: 0,
        filter: libc::EVFILT_USER,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut()
    };

    unsafe {
        libc::kevent(kq, &user_ev, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
    }



    loop {
        loop {
            match receiver.try_recv() { //This should be registered with  kevent too
                Ok(ThreadMessage::Shutdown)        => break,
                Ok(ThreadMessage::AddAcceptEvent{ fd, callback }) => {
                    println!("registering accept event for fd {}", fd);
                    readevents[fd as usize] = Some((CallbackType::ACCEPT(callback))); //TOOD: check whether it's better this way or to move it on to the context of the kevent

                    let ev_set = libc::kevent {
                        ident: fd as libc::uintptr_t,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut()
                    };

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Ok(ThreadMessage::AddReadEvent{ fd, callback }) => {
                    println!("added read event");
                    readevents[fd as usize] = Some((CallbackType::READ(callback)));

                    let ev_set = libc::kevent {
                        ident: fd as libc::uintptr_t,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut()
                    };

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Ok(ThreadMessage::Execute{ callback }) => {
                    callback();
                },
                Ok(ThreadMessage::Schedule{ delay, callback }) => {
                    let ev_set = libc::kevent {
                        ident: 0,
                        filter: libc::EVFILT_TIMER,
                        flags: libc::EV_ADD | libc::EV_ENABLE,
                        fflags: 0,
                        data: (delay.as_secs() * 1000 + (delay.subsec_nanos() / 1000000u32) as u64) as isize,
                        udata: std::ptr::null_mut()
                    };

                    timer = Some(callback);

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Err(_) => {
                    break;
                }
            }
        }

        unsafe {
            nev = libc::kevent(kq, std::ptr::null(), 0, ev_list.as_mut_ptr(), 32, std::ptr::null_mut());
        }

        match nev {
            -1   => println!("Error occured"),
            0   => {}, //println!("Fired without any events"),
            num => {
                //println!("got {} events", num);

                for i in  0..num {
                    let fd = ev_list[0].ident as i32;
                    let filt = ev_list[0].filter as i16;
                    match filt {
                        libc::EVFILT_READ => {
                            if readevents[fd as usize].is_some() {
                                let mut deleted = false;
                                if let Some(ref cb_type) = readevents[fd as usize] {
                                    match cb_type {
                                        &CallbackType::ACCEPT(ref cb)  => unsafe {
                                            println!("accepting from fd {}", fd);
                                            let mut listener = TcpListener::from_raw_fd(fd);
                                            match (cb)(&mut listener) {
                                                EventControl::DELETE => {
                                                    let ev_set = libc::kevent {
                                                        ident: fd as libc::uintptr_t,
                                                        filter: libc::EVFILT_READ,
                                                        flags: libc::EV_DELETE,
                                                        fflags: 0,
                                                        data: 0,
                                                        udata: std::ptr::null_mut()
                                                    };

                                                    libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                                    deleted = true;
                                                },
                                                EventControl::KEEP => {
                                                    listener.into_raw_fd();
                                                }
                                            }
                                        },
                                        &CallbackType::READ(ref cb) => unsafe {
                                            println!("reading from fd {}", fd);
                                            let mut stream = TcpStream::from_raw_fd(fd);
                                            match (cb)(&mut stream) {
                                                EventControl::DELETE => {
                                                    let ev_set = libc::kevent {
                                                        ident: fd as libc::uintptr_t,
                                                        filter: libc::EVFILT_READ,
                                                        flags: libc::EV_DELETE,
                                                        fflags: 0,
                                                        data: 0,
                                                        udata: std::ptr::null_mut()
                                                    };

                                                    libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                                    deleted = true;
                                                },
                                                EventControl::KEEP  => {
                                                    stream.into_raw_fd();
                                                }
                                            }
                                        }
                                    }
                                }
                                if deleted == true {
                                    readevents[fd as usize].take();
                                }
                            }
                        },
                        libc::EVFILT_TIMER => {
                            if let Some(ref c) = timer {
                                c();
                            }
                        }
                        libc::EVFILT_USER => {
                        }
                        x => {
                            println!("unhandled event {}", x);
                        }
                    }
                }
            }
        }
    }

    println!("Shutting down thread");
}

