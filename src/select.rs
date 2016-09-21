use super::{EventControl, libc, EventLoop, ReadEventType};
use super::future::Future;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::os::unix::io::IntoRawFd;
use std::os::unix::io::FromRawFd;
use std::ptr::*;
use std::mem::uninitialized;
use std::sync::atomic::AtomicUsize;

const MAX_TIMERS:usize = 4096;


pub struct Select {
    readevents: Arc<Mutex<Vec<Option<ReadEventType>>>>,
    writeevents: Arc<Mutex<Vec<Option<Box<Fn(&mut TcpStream) -> EventControl + Send>>>>>,
    timers: Arc<Mutex<Vec<Option<(Box<Fn() -> EventControl + Send>)>>>>,
    read_fds: libc::fd_set,
    write_fds: libc::fd_set,
    notify_pipe_fds: [libc::c_int; 2],
    next_timer: usize
}

unsafe impl Sync for Select {
    //Gotta actually make it threadsafe
}

impl Clone for Select {
    fn clone(&self) -> Select {
        Select {
            readevents: self.readevents.clone(),
            writeevents: self.writeevents.clone(),
            timers: self.timers.clone(),
            read_fds: self.read_fds, //TODO: clone?
            write_fds: self.write_fds, //TODO: clone?
            notify_pipe_fds: self.notify_pipe_fds,
            next_timer: self.next_timer
        }
    }
}

impl EventLoop for Select {

    fn new() -> Select {

        let num_fds: usize = unsafe {
                    let mut rlim: libc::rlimit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                    libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim as *mut libc::rlimit );
                    rlim.rlim_cur   //TODO: should we be using rlim_max?
        } as usize;

        let mut pipe_fds = [0; 2];
        unsafe {
            libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_NONBLOCK);
        }

        let mut kq = Select {
            readevents: Arc::new(Mutex::new(Vec::with_capacity(num_fds))), //TODO: maybe i should put it on the heap and stick it on to the event context
            writeevents: Arc::new(Mutex::new(Vec::with_capacity(num_fds))), //TODO: maybe i should put it on the heap and stick it on to the event context
            timers: Arc::new(Mutex::new(Vec::with_capacity(MAX_TIMERS))),  //TODO: maybe i should put it on the heap and stick it on to the event context
            read_fds: unsafe { uninitialized() },
            write_fds: unsafe { uninitialized() },
            notify_pipe_fds: pipe_fds,
            next_timer: 0
        };

        for _ in 0..num_fds {
            kq.readevents.lock().unwrap().push(None);
            kq.writeevents.lock().unwrap().push(None);
            kq.timers.lock().unwrap().push(None);
        }

        unsafe {
            libc::FD_ZERO(&mut kq.read_fds as *mut libc::fd_set);
            libc::FD_ZERO(&mut kq.write_fds as *mut libc::fd_set);
            libc::FD_SET(pipe_fds[0], &mut kq.read_fds as *mut libc::fd_set);
        }

        kq
    }

    fn add_read_event(&mut self, fd: usize, callback: ReadEventType) {
        println!("registering read event for fd {}", fd);
        self.readevents.lock().unwrap()[fd as usize] = Some(callback);

        unsafe {
            libc::FD_SET(fd as i32, &mut self.read_fds as *mut libc::fd_set);
        }
    }


    fn add_write_event(&mut self, fd: usize, callback: Box<Fn(&mut TcpStream) -> EventControl + Send>) {
        self.writeevents.lock().unwrap()[fd as usize] = Some(callback);
        debug!("adding write event to fd {}", fd);

        unsafe {
            libc::FD_SET(fd as i32, &mut self.write_fds as *mut libc::fd_set);
        }
    }

    fn add_timer(&mut self, callback: Box<Fn() -> EventControl + Send>, delay: Duration) {
        // add timer to the heap to calculate the next timeout for select
    }

    fn notify(&self) {
        println!("wake up bitch");
        let payload = [0;1];

        unsafe {
            libc::write(self.notify_pipe_fds[1], payload.as_ptr() as *const libc::c_void, 1);
        }
    }

    fn handle_events(&mut self) {
        let mut nev;

        println!("handling events");
        unsafe {
            nev = libc::select(libc::FD_SETSIZE as i32, &mut self.read_fds as *mut libc::fd_set, &mut self.write_fds as *mut libc::fd_set, null_mut(), null_mut());
        }

        match nev {
            -1   => println!("Error occured"),
            0   => {}, //println!("Fired without any events"),
            num => {
                println!("got {} events", num);

                for fd in  0..libc::FD_SETSIZE as i32 {

                    if unsafe { libc::FD_ISSET(fd, &mut self.read_fds as *mut libc::fd_set) } {
                        println!("got something on {} pipe is {}", fd, self.notify_pipe_fds[0]);
                        if fd == self.notify_pipe_fds[0] {
                            let mut buf = [0;10];
                            unsafe {libc::read(self.notify_pipe_fds[0], buf.as_mut_ptr() as *mut libc::c_void, 10)};
                            println!("read -> {:?}", buf);
                        } else {
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
                                                    unsafe { libc::FD_CLR(fd , &mut self.read_fds as *mut libc::fd_set) };
                                                    deleted = true;
                                                },
                                                EventControl::KEEP => {
                                                    listener.into_raw_fd();
                                                }
                                            }
                                        },
                                        &ReadEventType::READ(ref cb) => unsafe {
                                            println!("reading from fd {}", fd);
                                            let mut stream = TcpStream::from_raw_fd(fd );
                                            match (cb)(&mut stream) {
                                                EventControl::DELETE => {
                                                    unsafe { libc::FD_CLR(fd, &mut self.read_fds as *mut libc::fd_set) };
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
                        }

                    } else if unsafe { libc::FD_ISSET(fd, &mut self.write_fds as *mut libc::fd_set) } {
                            let mut locked_writeevents = self.writeevents.lock().unwrap();
                            if locked_writeevents[fd as usize].is_some() {
                                let mut deleted = false;
                                if let Some(ref cb) = locked_writeevents[fd as usize] {
                                    let mut stream = unsafe { TcpStream::from_raw_fd(fd) };
                                    match (cb)(&mut stream ) {
                                        EventControl::DELETE => {
                                            unsafe { libc::FD_CLR(fd, &mut self.write_fds as *mut libc::fd_set) };
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
                    }
                        //libc::EVFILT_WRITE => {
                                                    //},
                        //libc::EVFILT_TIMER => {
                            //let mut deleted = false;
                            //let mut locked_timers = self.timers.lock().unwrap();
                            //if let Some(ref c) = locked_timers[fd as usize] {
                                //match (*c)() {
                                    //EventControl::DELETE => {
                                        //let ev_set = libc::kevent {
                                            //ident: fd as libc::uintptr_t,
                                            //filter: libc::EVFILT_TIMER,
                                            //flags: libc::EV_DELETE,
                                            //fflags: 0,
                                            //data: 0,
                                            //udata: null_mut()
                                        //};

                                        //unsafe {
                                            //libc::kevent(self.kqid, &ev_set, 1, null_mut(), 0, null_mut());
                                        //}
                                        //deleted = true;
                                    //}
                                    //EventControl::KEEP => {
                                        ////noop
                                    //}
                                //}
                            //}

                            //if deleted == true {
                                //locked_timers[fd as usize].take();
                            //}
                        //}
                        //libc::EVFILT_USER => {
                            //println!("user event blah blah");
                        //}
                        //x => {
                            //println!("unhandled event {}", x);
                        //}
                    //}
                }
            }
        }

    }
}

