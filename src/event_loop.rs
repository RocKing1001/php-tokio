
use std::cell::RefCell;
use std::fs::File;
use std::future::Future;
use std::io::{self, Write};
use std::os::fd::{RawFd, FromRawFd};
use std::sync::mpsc::{Sender, Receiver, channel};
use std::io::Read;
use crate::borrow_unchecked::borrow_unchecked;
use ext_php_rs::boxed::ZBox;
use ext_php_rs::call_user_func;
use ext_php_rs::prelude::*;
use ext_php_rs::types::ZendHashTable;
use ext_php_rs::zend::Function;
use lazy_static::lazy_static;
use tokio::runtime::Runtime;
use std::os::fd::AsRawFd;

lazy_static! {
    pub static ref RUNTIME: Runtime = Runtime::new().expect("Could not allocate runtime");
}

thread_local! {
    static EVENTLOOP: RefCell<Option<EventLoop>> = RefCell::new(None);
}

#[cfg(any(target_os = "linux", target_os = "solaris"))]
fn sys_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut pipefd = [0; 2];
    let ret = unsafe { libc::pipe2(pipefd.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((pipefd[0], pipefd[1]))
}

pub struct EventLoop {
    fibers: ZBox<ZendHashTable>,

    sender: Sender<u64>,
    receiver: Receiver<u64>,

    notify_sender: File,
    notify_receiver: File,

    get_current_suspension: Function,

    dummy: [u8; 1],
}

impl EventLoop {
    pub fn init() -> PhpResult<u64> {
        EVENTLOOP.with_borrow_mut(|e| {
            Ok(
                match e {
                    None => e.insert(Self::new()?),
                    Some(ev) => ev
                }.notify_receiver.as_raw_fd() as u64
            )
        })
    }
    
    pub fn suspend_on<T: Send + 'static, F: Future<Output = T> + Send + 'static>(future: F) -> T {
        let (future, get_current_suspension) = EVENTLOOP.with_borrow_mut(move |c| {
            let c = c.as_mut().unwrap();
            let idx = c.fibers.len() as u64;
            c.fibers.insert_at_index(idx, call_user_func!(c.get_current_suspension).unwrap()).unwrap();

            let sender = c.sender.clone();
            let mut notifier = c.notify_sender.try_clone().unwrap();

            (RUNTIME.spawn(async move {
                let res = future.await;
                sender.send(idx).unwrap();
                notifier.write_all(&[0]).unwrap();
                res
            }), unsafe {
                borrow_unchecked(&c.get_current_suspension)
            })
        });

        call_user_func!(get_current_suspension).unwrap().try_call_method("suspend", vec![]).unwrap();

        return RUNTIME.block_on(future).unwrap();
    }
    
    pub fn wakeup() -> PhpResult<()> {
        EVENTLOOP.with_borrow_mut(|c| {
            let c = c.as_mut().unwrap();
            
            c.notify_receiver.read_exact(&mut c.dummy).unwrap();

            for fiber_id in c.receiver.try_iter() {
                if let Some(fiber) = c.fibers.get_index_mut(fiber_id) {
                    fiber.object_mut().unwrap().try_call_method("resume", vec![])?;
                    c.fibers.remove_index(fiber_id);
                }
            }
            Ok(())
        })
    }

    pub fn shutdown() {
        EVENTLOOP.set(None)
    }

    pub fn new() -> PhpResult<Self> {
        let (sender, receiver) = channel();
        let (notify_receiver, notify_sender) =
            sys_pipe().map_err(|err| format!("Could not create pipe: {}", err))?;

        if !call_user_func!(Function::from_function("class_exists"), "\\Revolt\\EventLoop")?.bool().unwrap_or(false) {
            return Err(format!("\\Revolt\\EventLoop does not exist!").into());
        }
        if !call_user_func!(Function::from_function("interface_exists"), "\\Revolt\\EventLoop\\Suspension")?.bool().unwrap_or(false) {
            return Err(format!("\\Revolt\\EventLoop\\Suspension does not exist!").into());
        }

        Ok(Self {
            fibers: ZendHashTable::new(),
            sender: sender,
            receiver: receiver,
            notify_sender: unsafe { File::from_raw_fd(notify_sender) },
            notify_receiver: unsafe { File::from_raw_fd(notify_receiver) },
            dummy: [0; 1],
            get_current_suspension: Function::try_from_method("\\Revolt\\EventLoop", "getSuspension").ok_or("\\Revolt\\EventLoop::getSuspension does not exist")?,
        })
    }
}

