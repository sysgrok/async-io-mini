use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Waker;

use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::MutexGuard;

use enumset::{EnumSet, EnumSetType};

use log::{debug, info, trace};

use libc as sys;

use crate::{syscall, syscall_los, syscall_los_eagain};

// In future, we might want to use a smaller - and possibly - configurable - with cargo feature(s)
// amount of registrations to save memory, but for now, let's use the maximum amount
const MAX_REGISTRATIONS: usize = sys::FD_SETSIZE;

#[derive(EnumSetType, Debug)]
pub(crate) enum Event {
    Read = 0,
    Write = 1,
}

struct Fds {
    read: MaybeUninit<sys::fd_set>,
    write: MaybeUninit<sys::fd_set>,
    except: MaybeUninit<sys::fd_set>,
}

impl Fds {
    const fn new() -> Self {
        Self {
            read: MaybeUninit::uninit(),
            write: MaybeUninit::uninit(),
            except: MaybeUninit::uninit(),
        }
    }

    fn zero(&mut self) {
        unsafe {
            sys::FD_ZERO(self.read.as_mut_ptr());
            sys::FD_ZERO(self.write.as_mut_ptr());
            sys::FD_ZERO(self.except.as_mut_ptr());
        }
    }

    fn is_set(&self, fd: RawFd, event: Event) -> bool {
        unsafe { sys::FD_ISSET(fd, self.fd_set(event)) }
    }

    fn set(&mut self, fd: RawFd, event: Event) {
        unsafe { sys::FD_SET(fd, self.fd_set_mut(event)) }
    }

    fn fd_set(&self, event: Event) -> &sys::fd_set {
        unsafe {
            match event {
                Event::Read => self.read.assume_init_ref(),
                Event::Write => self.write.assume_init_ref(),
            }
        }
    }

    fn fd_set_mut(&mut self, event: Event) -> &mut sys::fd_set {
        unsafe {
            match event {
                Event::Read => self.read.assume_init_mut(),
                Event::Write => self.write.assume_init_mut(),
            }
        }
    }
}

struct Registration {
    fd: RawFd,
    events: EnumSet<Event>,
    wakers: [Option<Waker>; 2],
}

struct Registrations<const N: usize> {
    vec: heapless::Vec<Registration, N>,
    event_fd: Option<OwnedFd>,
    waiting: usize,
}

impl<const N: usize> Registrations<N> {
    const fn new() -> Self {
        Self {
            vec: heapless::Vec::new(),
            event_fd: None,
            waiting: 0,
        }
    }

    fn register(&mut self, fd: RawFd) -> io::Result<()> {
        if fd < 0
            || self
                .event_fd
                .as_ref()
                .map(|event_fd| fd == event_fd.as_raw_fd())
                .unwrap_or(false)
        {
            Err(ErrorKind::InvalidInput)?;
        }

        if fd >= sys::FD_SETSIZE as RawFd {
            Err(ErrorKind::InvalidInput)?;
        }

        if self.vec.iter().any(|reg| reg.fd == fd) {
            Err(ErrorKind::InvalidInput)?;
        }

        self.vec
            .push(Registration {
                fd,
                events: EnumSet::empty(),
                wakers: [None, None],
            })
            .map_err(|_| ErrorKind::OutOfMemory)?;

        Ok(())
    }

    fn deregister(&mut self, fd: RawFd) -> io::Result<()> {
        let Some(index) = self.vec.iter_mut().position(|reg| reg.fd == fd) else {
            return Err(ErrorKind::NotFound.into());
        };

        self.vec.swap_remove(index);

        Ok(())
    }

    fn set(&mut self, fd: RawFd, event: Event, waker: &Waker) -> io::Result<()> {
        let Some(registration) = self.vec.iter_mut().find(|reg| reg.fd == fd) else {
            return Err(ErrorKind::NotFound.into());
        };

        registration.events.remove(event);

        if let Some(prev_waker) = registration.wakers[event as usize].replace(waker.clone()) {
            if !prev_waker.will_wake(waker) {
                prev_waker.wake();
            }
        }

        Ok(())
    }

    fn fetch(&mut self, fd: RawFd, event: Event) -> io::Result<bool> {
        let Some(registration) = self.vec.iter_mut().find(|reg| reg.fd == fd) else {
            return Err(ErrorKind::NotFound.into());
        };

        let set = registration.events.contains(event);

        registration.events.remove(event);

        Ok(set)
    }

    #[allow(deprecated)]
    fn set_fds(&self, fds: &mut Fds) -> io::Result<Option<RawFd>> {
        fds.zero();

        let mut max: Option<RawFd> = None;

        if let Some(event_fd) = self.event_fd.as_ref().map(|event_fd| event_fd.as_raw_fd()) {
            fds.set(event_fd, Event::Read);
            max = Some(max.map_or(event_fd, |max| max.max(event_fd)));

            trace!("Set event FD: {event_fd}");
        }

        for registration in &self.vec {
            for event in EnumSet::ALL {
                if registration.wakers[event as usize].is_some() {
                    fds.set(registration.fd, event);

                    trace!("Set registration FD: {}/{event:?}", registration.fd);
                }

                max = Some(max.map_or(registration.fd, |max| max.max(registration.fd)));
            }
        }

        trace!("Max FDs: {max:?}");

        Ok(max)
    }

    #[allow(deprecated)]
    fn update_events(&mut self, fds: &Fds) -> io::Result<()> {
        trace!("Updating events");

        self.consume_notification()?;

        for registration in &mut self.vec {
            for event in EnumSet::ALL {
                if fds.is_set(registration.fd, event) {
                    trace!("Registration FD is set: {}/{event:?}", registration.fd);

                    registration.events |= event;
                    if let Some(waker) = registration.wakers[event as usize].take() {
                        waker.wake();
                    }
                }
            }
        }

        Ok(())
    }

    fn create_notification(&mut self) -> io::Result<bool> {
        if self.event_fd.is_none() {
            #[cfg(not(target_os = "espidf"))]
            let event_fd =
                unsafe { OwnedFd::from_raw_fd(syscall_los!(sys::eventfd(0, sys::EFD_NONBLOCK))?) };

            // Note that the eventfd() implementation in ESP-IDF deviates from the specification in the following ways:
            // 1) The file descriptor is always in a non-blocking mode, as if EFD_NONBLOCK was passed as a flag;
            //    passing EFD_NONBLOCK or calling fcntl(.., F_GETFL/F_SETFL) on the eventfd() file descriptor is not supported
            // 2) It always returns the counter value, even if it is 0. This is contrary to the specification which mandates
            //    that it should instead fail with EAGAIN
            //
            // (1) is not a problem for us, as we want the eventfd() file descriptor to be in a non-blocking mode anyway
            // (2) is also not a problem, as long as we don't try to read the counter value in an endless loop when we detect being notified
            #[cfg(target_os = "espidf")]
            let event_fd = unsafe {
                OwnedFd::from_raw_fd(syscall_los!(sys::eventfd(0, 0)).map_err(|err| {
                    match err {
                        err if err.kind() == io::ErrorKind::PermissionDenied => {
                            // EPERM can happen if the eventfd isn't initialized yet.
                            // Tell the user to call esp_vfs_eventfd_register.
                            io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "failed to initialize eventfd for polling, try calling `esp_vfs_eventfd_register`"
                            )
                        },
                        err => err,
                    }
                })?)
            };

            debug!("Created event FD: {}", event_fd.as_raw_fd());

            self.event_fd = Some(event_fd);

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn destroy_notification(&mut self) -> io::Result<bool> {
        if let Some(event_fd) = self.event_fd.take() {
            syscall!(unsafe { sys::close(event_fd.as_raw_fd()) })?;

            debug!("Closed event FD: {}", event_fd.as_raw_fd());

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn notify(&self) -> io::Result<bool> {
        if let Some(event_fd) = self.event_fd.as_ref() {
            let event_fd = event_fd.as_raw_fd();

            syscall_los_eagain!(unsafe {
                sys::write(
                    event_fd,
                    &u64::to_be_bytes(1_u64) as *const _ as *const _,
                    core::mem::size_of::<u64>(),
                )
            })?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn consume_notification(&mut self) -> io::Result<bool> {
        if let Some(event_fd) = self.event_fd.as_ref() {
            let event_fd = event_fd.as_raw_fd();

            let mut buf = [0_u8; core::mem::size_of::<u64>()];

            syscall_los_eagain!(unsafe {
                sys::read(
                    event_fd,
                    &mut buf as *mut _ as *mut _,
                    core::mem::size_of::<u64>(),
                )
            })?;

            trace!("Consumed notification");

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub struct Reactor<const N: usize> {
    registrations: std::sync::Mutex<Registrations<N>>,
    condvar: std::sync::Condvar,
    started: AtomicBool,
}

impl<const N: usize> Reactor<N> {
    const fn new() -> Self {
        Self {
            registrations: std::sync::Mutex::new(Registrations::new()),
            condvar: std::sync::Condvar::new(),
            started: AtomicBool::new(false),
        }
    }

    /// Starts the reactor. Returns `false` if it had been already started.
    pub fn start(&'static self) -> io::Result<bool> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(false);
        }

        info!("Starting reactor");

        std::thread::Builder::new()
            .name("async-io-mini".into())
            .stack_size(3048)
            .spawn(move || {
                self.run().unwrap();
            })?;

        Ok(true)
    }

    pub(crate) fn register(&self, fd: RawFd) -> io::Result<()> {
        self.modify(|regs| regs.register(fd))
    }

    pub(crate) fn deregister(&self, fd: RawFd) -> io::Result<()> {
        self.modify(|regs| regs.deregister(fd))
    }

    // pub(crate) fn set(&self, fd: RawFd, event: Event, waker: &Waker) -> io::Result<()> {
    //     self.lock(|regs| regs.set(fd, event, waker))
    // }

    pub(crate) fn fetch(&self, fd: RawFd, event: Event) -> io::Result<bool> {
        self.modify(|regs| regs.fetch(fd, event))
    }

    pub(crate) fn fetch_or_set(&self, fd: RawFd, event: Event, waker: &Waker) -> io::Result<bool> {
        self.modify(|regs| {
            if regs.fetch(fd, event)? {
                Ok(true)
            } else {
                regs.set(fd, event, waker)?;

                Ok(false)
            }
        })
    }

    fn run(&self) -> io::Result<()> {
        if !self.lock(|mut guard| guard.create_notification())? {
            Err(ErrorKind::AlreadyExists)?;
        }

        debug!("Running");

        let mut fds = Fds::new();
        let mut update = false;

        let result = loop {
            let max = self.apply(|inner| {
                if !update {
                    update = true;
                } else {
                    inner.update_events(&fds)?;
                }

                inner.set_fds(&mut fds)
            });

            let result = match max {
                Err(err) => Err(err),
                Ok(None) => unreachable!("EventFD is not there?"),
                Ok(Some(max)) => {
                    trace!("Start select");

                    let result = syscall_los!(unsafe {
                        sys::select(
                            max + 1,
                            fds.read.assume_init_mut(),
                            fds.write.assume_init_mut(),
                            fds.except.assume_init_mut(),
                            core::ptr::null_mut(),
                        )
                    });

                    trace!("End select");

                    result.map(|_| ())
                }
            };

            if result.is_err() {
                break result;
            }
        };

        if !self.lock(|mut guard| guard.destroy_notification())? {
            Err(ErrorKind::NotFound)?;
        }

        result
    }

    fn modify<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Registrations<N>) -> io::Result<R>,
    {
        self.lock(|mut guard| {
            guard.waiting += 1;

            let result = f(&mut guard);

            guard.notify()?;

            let _guard = self
                .condvar
                .wait_while(guard, |registrations| registrations.waiting > 0)
                .unwrap();

            result
        })
    }

    fn apply<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Registrations<N>) -> io::Result<R>,
    {
        self.lock(|mut guard| {
            let result = f(&mut guard);

            guard.waiting = 0;

            self.condvar.notify_all();

            result
        })
    }

    fn lock<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(MutexGuard<Registrations<N>>) -> io::Result<R>,
    {
        f(self.registrations.lock().unwrap())
    }
}

pub static REACTOR: Reactor<MAX_REGISTRATIONS> = Reactor::new();
