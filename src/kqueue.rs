use super::{EventControl, libc};
use super::future::Future;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::os::unix::io::IntoRawFd;
use std::os::unix::io::FromRawFd;
use std::ptr::*;
use std::sync::atomic::AtomicUsize;

const MAX_TIMERS:usize = 4096;

pub enum ReadEventType {
    ACCEPT(Box<Fn(&mut TcpListener) -> EventControl + Send>),
    READ(Box<Fn(&mut TcpStream) -> EventControl + Send>)
}

pub struct Kqueue {
    kqid: i32,
    readevents: Arc<Mutex<Vec<Option<ReadEventType>>>>,
    writeevents: Arc<Mutex<Vec<Option<Box<Fn(&mut TcpStream) -> EventControl + Send>>>>>,
    timers: Arc<Mutex<Vec<Option<(Box<Fn() -> EventControl + Send>)>>>>,
    next_timer: usize
}

unsafe impl Sync for Kqueue {
    //Gotta actually make it threadsafe
}

impl Clone for Kqueue {
    fn clone(&self) -> Kqueue {
        Kqueue {
            kqid: self.kqid,
            readevents: self.readevents.clone(),
            writeevents: self.writeevents.clone(),
            timers: self.timers.clone(),
            next_timer: self.next_timer
        }
    }
}

impl Kqueue {

    pub fn new() -> Kqueue {

        let num_fds: usize = unsafe {
                    let mut rlim: libc::rlimit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                    libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim as *mut libc::rlimit );
                    rlim.rlim_cur   //TODO: should we be using rlim_max?
        } as usize;

        let mut kq = Kqueue {
            kqid : unsafe { libc::kqueue() as i32 },
            readevents: Arc::new(Mutex::new(Vec::with_capacity(num_fds))), //TODO: maybe i should put it on the heap and stick it on to the event context
            writeevents: Arc::new(Mutex::new(Vec::with_capacity(num_fds))), //TODO: maybe i should put it on the heap and stick it on to the event context
            timers: Arc::new(Mutex::new(Vec::with_capacity(MAX_TIMERS))),  //TODO: maybe i should put it on the heap and stick it on to the event context
            next_timer: 0
        };

        for _ in 0..num_fds {
            kq.readevents.lock().unwrap().push(None);
            kq.writeevents.lock().unwrap().push(None);
            kq.timers.lock().unwrap().push(None);
        }

        let user_ev = libc::kevent {
            ident: 0,
            filter: libc::EVFILT_USER,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: null_mut()
        };

        unsafe {
            libc::kevent(kq.kqid, &user_ev, 1, null_mut(), 0, null_mut());
        }

        kq
    }

    pub fn add_read_event(&mut self, fd: usize, callback: ReadEventType) {
        println!("registering read event for fd {}", fd);
        self.readevents.lock().unwrap()[fd as usize] = Some(callback);

        let ev_set = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD,
            fflags: 0,
            data: 0,
            udata: null_mut()
        };

        unsafe {
            libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
        }
    }


    pub fn add_write_event(&mut self, fd: usize, callback: Box<Fn(&mut TcpStream) -> EventControl + Send>) {
        self.writeevents.lock().unwrap()[fd as usize] = Some(callback);
        debug!("adding write event to fd {}", fd);

        let ev_set = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_WRITE,
            flags: libc::EV_ADD,
            fflags: 0,
            data: 0,
            udata: null_mut()
        };

        unsafe {
            libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
        }
    }

    pub fn add_timer(&mut self, callback: Box<Fn() -> EventControl + Send>, delay: Duration) {
        let mut locked_timers = self.timers.lock().unwrap();
        while let Some(_) = locked_timers[self.next_timer] {
            self.next_timer = (self.next_timer+ 1) % MAX_TIMERS;
        }
        locked_timers[self.next_timer] = Some(callback);

        let ev_set = libc::kevent {
            ident: self.next_timer,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ENABLE,
            fflags: 0,
            data: (delay.as_secs() * 1000 + (delay.subsec_nanos() / 1000000u32) as u64) as isize,
            udata: null_mut()
        };

        unsafe {
            libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
        }
    }

    pub fn notify(&self) {
        let ev = libc::kevent {
            ident: 0,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: null_mut()
        };

        unsafe {
            libc::kevent(self.kqid, &ev, 1, null_mut(), 0, null_mut());
        }
    }

    pub fn handle_events(&mut self) {
        let mut nev;
        let mut ev_list: [libc::kevent; 32] = [ libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: null_mut() };32];

        unsafe {
            nev = libc::kevent(self.kqid, null(), 0, ev_list.as_mut_ptr(), 32, null_mut());
        }

        match nev {
            -1   => println!("Error occured"),
            0   => {}, //println!("Fired without any events"),
            num => {
                println!("got {} events", num);

                for i in  0..num {
                    let fd = ev_list[i as usize].ident as i32;
                    let filt = ev_list[i as usize].filter as i16;
                    match filt {
                        libc::EVFILT_READ => {
                            let mut locked_readevents = self.readevents.lock().unwrap();
                            if locked_readevents[fd as usize].is_some() {
                                let mut deleted = false;
                                if let Some(ref cb_type) = locked_readevents[fd as usize] {
                                    match cb_type {
                                        &ReadEventType::ACCEPT(ref cb)  => unsafe {
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
                                                        udata: null_mut()
                                                    };

                                                    libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
                                                    deleted = true;
                                                },
                                                EventControl::KEEP => {
                                                    listener.into_raw_fd();
                                                }
                                            }
                                        },
                                        &ReadEventType::READ(ref cb) => unsafe {
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
                                                        udata: null_mut()
                                                    };

                                                    libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
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
                                    locked_readevents[fd as usize].take();
                                }
                            }
                        },
                        libc::EVFILT_WRITE => {
                            //TODO: allow for custom write routines to be registered and used

                            let mut locked_writeevents = self.writeevents.lock().unwrap();
                            if locked_writeevents[fd as usize].is_some() {
                                let mut deleted = false;
                                if let Some(ref cb) = locked_writeevents[fd as usize] {
                                    let mut stream = unsafe { TcpStream::from_raw_fd(fd) };
                                    match (cb)(&mut stream ) {
                                        EventControl::DELETE => {
                                            let ev_set = libc::kevent {
                                                ident: fd as libc::uintptr_t,
                                                filter: libc::EVFILT_WRITE,
                                                flags: libc::EV_DELETE,
                                                fflags: 0,
                                                data: 0,
                                                udata: null_mut()
                                            };

                                            unsafe { libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut()) };
                                            deleted = true;
                                        },
                                        EventControl::KEEP => {
                                            stream.into_raw_fd();
                                        }
                                    }
                                }

                                if deleted == true {
                                    locked_writeevents[fd as usize].take();
                                }
                            }
                        },
                        libc::EVFILT_TIMER => {
                            let mut deleted = false;
                            let mut locked_timers = self.timers.lock().unwrap();
                            if let Some(ref c) = locked_timers[fd as usize] {
                                match (*c)() {
                                    EventControl::DELETE => {
                                        let ev_set = libc::kevent {
                                            ident: fd as libc::uintptr_t,
                                            filter: libc::EVFILT_TIMER,
                                            flags: libc::EV_DELETE,
                                            fflags: 0,
                                            data: 0,
                                            udata: null_mut()
                                        };

                                        unsafe {
                                            libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
                                        }
                                        deleted = true;
                                    }
                                    EventControl::KEEP => {
                                        //noop
                                    }
                                }
                            }

                            if deleted == true {
                                locked_timers[fd as usize].take();
                            }
                        }
                        libc::EVFILT_USER => {
                            println!("user event blah blah");
                        }
                        x => {
                            println!("unhandled event {}", x);
                        }
                    }
                }
            }
        }

    }
}

