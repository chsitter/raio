use std::sync::{Arc, Mutex, Condvar};

pub struct Future {
    condvar: Arc<((Mutex<bool>, Condvar))>
}

impl Clone for Future {
    fn clone(&self) -> Future {
        Future {
            condvar: self.condvar.clone()
        }
    }
}

impl Future {

    pub fn new() -> Future {
        Future {
            condvar: Arc::new((Mutex::new(false), Condvar::new()))
        }
    }

    pub fn get(self) {
        let &(ref lock, ref cvar) = &*self.condvar;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }

    }

    pub fn set(&mut self) {
        let &(ref lock, ref cvar) = &*self.condvar;
        let mut started = lock.lock().unwrap();
        *started = true;
        cvar.notify_one();
    }
}
