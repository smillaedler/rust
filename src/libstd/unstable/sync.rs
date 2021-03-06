// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use cast;
use cell::Cell;
use comm;
use libc;
use ptr;
use option::*;
use either::{Either, Left, Right};
use task;
use task::atomically;
use unstable::atomics::{AtomicOption,AtomicUint,Acquire,Release,SeqCst};
use unstable::finally::Finally;
use ops::Drop;
use clone::Clone;
use kinds::Send;

/// An atomically reference counted pointer.
///
/// Enforces no shared-memory safety.
pub struct UnsafeAtomicRcBox<T> {
    data: *mut libc::c_void,
}

struct AtomicRcBoxData<T> {
    count: AtomicUint,
    // An unwrapper uses this protocol to communicate with the "other" task that
    // drops the last refcount on an arc. Unfortunately this can't be a proper
    // pipe protocol because the unwrapper has to access both stages at once.
    // FIXME(#7544): Maybe use AtomicPtr instead (to avoid xchg in take() later)?
    unwrapper: AtomicOption<(comm::ChanOne<()>, comm::PortOne<bool>)>,
    // FIXME(#3224) should be able to make this non-option to save memory
    data: Option<T>,
}

impl<T: Send> UnsafeAtomicRcBox<T> {
    pub fn new(data: T) -> UnsafeAtomicRcBox<T> {
        unsafe {
            let data = ~AtomicRcBoxData { count: AtomicUint::new(1),
                                          unwrapper: AtomicOption::empty(),
                                          data: Some(data) };
            let ptr = cast::transmute(data);
            return UnsafeAtomicRcBox { data: ptr };
        }
    }

    /// As new(), but returns an extra pre-cloned handle.
    pub fn new2(data: T) -> (UnsafeAtomicRcBox<T>, UnsafeAtomicRcBox<T>) {
        unsafe {
            let data = ~AtomicRcBoxData { count: AtomicUint::new(2),
                                          unwrapper: AtomicOption::empty(),
                                          data: Some(data) };
            let ptr = cast::transmute(data);
            return (UnsafeAtomicRcBox { data: ptr },
                    UnsafeAtomicRcBox { data: ptr });
        }
    }

    #[inline]
    pub unsafe fn get(&self) -> *mut T
    {
        let mut data: ~AtomicRcBoxData<T> = cast::transmute(self.data);
        assert!(data.count.load(Acquire) > 0); // no barrier is really needed
        let r: *mut T = data.data.get_mut_ref();
        cast::forget(data);
        return r;
    }

    #[inline]
    pub unsafe fn get_immut(&self) -> *T
    {
        let data: ~AtomicRcBoxData<T> = cast::transmute(self.data);
        assert!(data.count.load(Acquire) > 0); // no barrier is really needed
        let r: *T = data.data.get_ref();
        cast::forget(data);
        return r;
    }

    /// Wait until all other handles are dropped, then retrieve the enclosed
    /// data. See extra::arc::ARC for specific semantics documentation.
    /// If called when the task is already unkillable, unwrap will unkillably
    /// block; otherwise, an unwrapping task can be killed by linked failure.
    pub unsafe fn unwrap(self) -> T {
        let this = Cell::new(self); // argh
        do task::unkillable {
            let mut this = this.take();
            let mut data: ~AtomicRcBoxData<T> = cast::transmute(this.data);
            // Set up the unwrap protocol.
            let (p1,c1) = comm::oneshot(); // ()
            let (p2,c2) = comm::oneshot(); // bool
            // Try to put our server end in the unwrapper slot.
            // This needs no barrier -- it's protected by the release barrier on
            // the xadd, and the acquire+release barrier in the destructor's xadd.
            // FIXME(#6598) Change Acquire to Relaxed.
            if data.unwrapper.fill(~(c1,p2), Acquire).is_none() {
                // Got in. Tell this handle's destructor not to run (we are now it).
                this.data = ptr::mut_null();
                // Drop our own reference.
                let old_count = data.count.fetch_sub(1, Release);
                assert!(old_count >= 1);
                if old_count == 1 {
                    // We were the last owner. Can unwrap immediately.
                    // AtomicOption's destructor will free the server endpoint.
                    // FIXME(#3224): it should be like this
                    // let ~AtomicRcBoxData { data: user_data, _ } = data;
                    // user_data
                    data.data.take_unwrap()
                } else {
                    // The *next* person who sees the refcount hit 0 will wake us.
                    let p1 = Cell::new(p1); // argh
                    // Unlike the above one, this cell is necessary. It will get
                    // taken either in the do block or in the finally block.
                    let c2_and_data = Cell::new((c2,data));
                    do (|| {
                        do task::rekillable { p1.take().recv(); }
                        // Got here. Back in the 'unkillable' without getting killed.
                        let (c2, data) = c2_and_data.take();
                        c2.send(true);
                        // FIXME(#3224): it should be like this
                        // let ~AtomicRcBoxData { data: user_data, _ } = data;
                        // user_data
                        let mut data = data;
                        data.data.take_unwrap()
                    }).finally {
                        if task::failing() {
                            // Killed during wait. Because this might happen while
                            // someone else still holds a reference, we can't free
                            // the data now; the "other" last refcount will free it.
                            let (c2, data) = c2_and_data.take();
                            c2.send(false);
                            cast::forget(data);
                        } else {
                            assert!(c2_and_data.is_empty());
                        }
                    }
                }
            } else {
                // If 'put' returns the server end back to us, we were rejected;
                // someone else was trying to unwrap. Avoid guaranteed deadlock.
                cast::forget(data);
                fail!("Another task is already unwrapping this ARC!");
            }
        }
    }

    /// As unwrap above, but without blocking. Returns 'Left(self)' if this is
    /// not the last reference; 'Right(unwrapped_data)' if so.
    pub unsafe fn try_unwrap(self) -> Either<UnsafeAtomicRcBox<T>, T> {
        let mut this = self; // FIXME(#4330) mutable self
        let mut data: ~AtomicRcBoxData<T> = cast::transmute(this.data);
        // This can of course race with anybody else who has a handle, but in
        // such a case, the returned count will always be at least 2. If we
        // see 1, no race was possible. All that matters is 1 or not-1.
        let count = data.count.load(Acquire);
        assert!(count >= 1);
        // The more interesting race is one with an unwrapper. They may have
        // already dropped their count -- but if so, the unwrapper pointer
        // will have been set first, which the barriers ensure we will see.
        // (Note: using is_empty(), not take(), to not free the unwrapper.)
        if count == 1 && data.unwrapper.is_empty(Acquire) {
            // Tell this handle's destructor not to run (we are now it).
            this.data = ptr::mut_null();
            // FIXME(#3224) as above
            Right(data.data.take_unwrap())
        } else {
            cast::forget(data);
            Left(this)
        }
    }
}

impl<T: Send> Clone for UnsafeAtomicRcBox<T> {
    fn clone(&self) -> UnsafeAtomicRcBox<T> {
        unsafe {
            let mut data: ~AtomicRcBoxData<T> = cast::transmute(self.data);
            // This barrier might be unnecessary, but I'm not sure...
            let old_count = data.count.fetch_add(1, Acquire);
            assert!(old_count >= 1);
            cast::forget(data);
            return UnsafeAtomicRcBox { data: self.data };
        }
    }
}

#[unsafe_destructor]
impl<T> Drop for UnsafeAtomicRcBox<T>{
    fn drop(&self) {
        unsafe {
            if self.data.is_null() {
                return; // Happens when destructing an unwrapper's handle.
            }
            do task::unkillable {
                let mut data: ~AtomicRcBoxData<T> = cast::transmute(self.data);
                // Must be acquire+release, not just release, to make sure this
                // doesn't get reordered to after the unwrapper pointer load.
                let old_count = data.count.fetch_sub(1, SeqCst);
                assert!(old_count >= 1);
                if old_count == 1 {
                    // Were we really last, or should we hand off to an
                    // unwrapper? It's safe to not xchg because the unwrapper
                    // will set the unwrap lock *before* dropping his/her
                    // reference. In effect, being here means we're the only
                    // *awake* task with the data.
                    match data.unwrapper.take(Acquire) {
                        Some(~(message,response)) => {
                            // Send 'ready' and wait for a response.
                            message.send(());
                            // Unkillable wait. Message guaranteed to come.
                            if response.recv() {
                                // Other task got the data.
                                cast::forget(data);
                            } else {
                                // Other task was killed. drop glue takes over.
                            }
                        }
                        None => {
                            // drop glue takes over.
                        }
                    }
                } else {
                    cast::forget(data);
                }
            }
        }
    }
}


/****************************************************************************/

#[allow(non_camel_case_types)] // runtime type
type rust_little_lock = *libc::c_void;

pub struct LittleLock {
    l: rust_little_lock,
}

impl Drop for LittleLock {
    fn drop(&self) {
        unsafe {
            rust_destroy_little_lock(self.l);
        }
    }
}

pub fn LittleLock() -> LittleLock {
    unsafe {
        LittleLock {
            l: rust_create_little_lock()
        }
    }
}

impl LittleLock {
    #[inline]
    pub unsafe fn lock<T>(&self, f: &fn() -> T) -> T {
        do atomically {
            rust_lock_little_lock(self.l);
            do (|| {
                f()
            }).finally {
                rust_unlock_little_lock(self.l);
            }
        }
    }
}

struct ExData<T> {
    lock: LittleLock,
    failed: bool,
    data: T,
}

/**
 * An arc over mutable data that is protected by a lock. For library use only.
 *
 * # Safety note
 *
 * This uses a pthread mutex, not one that's aware of the userspace scheduler.
 * The user of an exclusive must be careful not to invoke any functions that may
 * reschedule the task while holding the lock, or deadlock may result. If you
 * need to block or yield while accessing shared state, use extra::sync::RWARC.
 */
pub struct Exclusive<T> {
    x: UnsafeAtomicRcBox<ExData<T>>
}

pub fn exclusive<T:Send>(user_data: T) -> Exclusive<T> {
    let data = ExData {
        lock: LittleLock(),
        failed: false,
        data: user_data
    };
    Exclusive {
        x: UnsafeAtomicRcBox::new(data)
    }
}

impl<T:Send> Clone for Exclusive<T> {
    // Duplicate an exclusive ARC, as std::arc::clone.
    fn clone(&self) -> Exclusive<T> {
        Exclusive { x: self.x.clone() }
    }
}

impl<T:Send> Exclusive<T> {
    // Exactly like std::arc::mutex_arc,access(), but with the little_lock
    // instead of a proper mutex. Same reason for being unsafe.
    //
    // Currently, scheduling operations (i.e., yielding, receiving on a pipe,
    // accessing the provided condition variable) are prohibited while inside
    // the exclusive. Supporting that is a work in progress.
    #[inline]
    pub unsafe fn with<U>(&self, f: &fn(x: &mut T) -> U) -> U {
        let rec = self.x.get();
        do (*rec).lock.lock {
            if (*rec).failed {
                fail!("Poisoned exclusive - another task failed inside!");
            }
            (*rec).failed = true;
            let result = f(&mut (*rec).data);
            (*rec).failed = false;
            result
        }
    }

    #[inline]
    pub unsafe fn with_imm<U>(&self, f: &fn(x: &T) -> U) -> U {
        do self.with |x| {
            f(cast::transmute_immut(x))
        }
    }

    pub fn unwrap(self) -> T {
        let Exclusive { x: x } = self;
        // Someday we might need to unkillably unwrap an exclusive, but not today.
        let inner = unsafe { x.unwrap() };
        let ExData { data: user_data, _ } = inner; // will destroy the LittleLock
        user_data
    }
}

extern {
    fn rust_create_little_lock() -> rust_little_lock;
    fn rust_destroy_little_lock(lock: rust_little_lock);
    fn rust_lock_little_lock(lock: rust_little_lock);
    fn rust_unlock_little_lock(lock: rust_little_lock);
}

#[cfg(test)]
mod tests {
    use cell::Cell;
    use comm;
    use option::*;
    use super::{exclusive, UnsafeAtomicRcBox};
    use task;
    use uint;
    use util;

    #[test]
    fn exclusive_arc() {
        unsafe {
            let mut futures = ~[];

            let num_tasks = 10;
            let count = 10;

            let total = exclusive(~0);

            for uint::range(0, num_tasks) |_i| {
                let total = total.clone();
                let (port, chan) = comm::stream();
                futures.push(port);

                do task::spawn || {
                    for uint::range(0, count) |_i| {
                        do total.with |count| {
                            **count += 1;
                        }
                    }
                    chan.send(());
                }
            };

            for futures.iter().advance |f| { f.recv() }

            do total.with |total| {
                assert!(**total == num_tasks * count)
            };
        }
    }

    #[test] #[should_fail] #[ignore(cfg(windows))]
    fn exclusive_poison() {
        unsafe {
            // Tests that if one task fails inside of an exclusive, subsequent
            // accesses will also fail.
            let x = exclusive(1);
            let x2 = x.clone();
            do task::try || {
                do x2.with |one| {
                    assert_eq!(*one, 2);
                }
            };
            do x.with |one| {
                assert_eq!(*one, 1);
            }
        }
    }

    #[test]
    fn arclike_unwrap_basic() {
        unsafe {
            let x = UnsafeAtomicRcBox::new(~~"hello");
            assert!(x.unwrap() == ~~"hello");
        }
    }

    #[test]
    fn arclike_try_unwrap() {
        unsafe {
            let x = UnsafeAtomicRcBox::new(~~"hello");
            assert!(x.try_unwrap().expect_right("try_unwrap failed") == ~~"hello");
        }
    }

    #[test]
    fn arclike_try_unwrap_fail() {
        unsafe {
            let x = UnsafeAtomicRcBox::new(~~"hello");
            let x2 = x.clone();
            let left_x = x.try_unwrap();
            assert!(left_x.is_left());
            util::ignore(left_x);
            assert!(x2.try_unwrap().expect_right("try_unwrap none") == ~~"hello");
        }
    }

    #[test]
    fn arclike_try_unwrap_unwrap_race() {
        // When an unwrap and a try_unwrap race, the unwrapper should always win.
        unsafe {
            let x = UnsafeAtomicRcBox::new(~~"hello");
            let x2 = Cell::new(x.clone());
            let (p,c) = comm::stream();
            do task::spawn {
                c.send(());
                assert!(x2.take().unwrap() == ~~"hello");
                c.send(());
            }
            p.recv();
            task::yield(); // Try to make the unwrapper get blocked first.
            let left_x = x.try_unwrap();
            assert!(left_x.is_left());
            util::ignore(left_x);
            p.recv();
        }
    }

    #[test]
    fn exclusive_unwrap_basic() {
        // Unlike the above, also tests no double-freeing of the LittleLock.
        let x = exclusive(~~"hello");
        assert!(x.unwrap() == ~~"hello");
    }

    #[test]
    fn exclusive_unwrap_contended() {
        let x = exclusive(~~"hello");
        let x2 = Cell::new(x.clone());
        do task::spawn {
            let x2 = x2.take();
            unsafe { do x2.with |_hello| { } }
            task::yield();
        }
        assert!(x.unwrap() == ~~"hello");

        // Now try the same thing, but with the child task blocking.
        let x = exclusive(~~"hello");
        let x2 = Cell::new(x.clone());
        let mut res = None;
        let mut builder = task::task();
        builder.future_result(|r| res = Some(r));
        do builder.spawn {
            let x2 = x2.take();
            assert!(x2.unwrap() == ~~"hello");
        }
        // Have to get rid of our reference before blocking.
        util::ignore(x);
        res.unwrap().recv();
    }

    #[test] #[should_fail] #[ignore(cfg(windows))]
    fn exclusive_unwrap_conflict() {
        let x = exclusive(~~"hello");
        let x2 = Cell::new(x.clone());
        let mut res = None;
        let mut builder = task::task();
        builder.future_result(|r| res = Some(r));
        do builder.spawn {
            let x2 = x2.take();
            assert!(x2.unwrap() == ~~"hello");
        }
        assert!(x.unwrap() == ~~"hello");
        // See #4689 for why this can't be just "res.recv()".
        assert!(res.unwrap().recv() == task::Success);
    }

    #[test] #[ignore(cfg(windows))]
    fn exclusive_unwrap_deadlock() {
        // This is not guaranteed to get to the deadlock before being killed,
        // but it will show up sometimes, and if the deadlock were not there,
        // the test would nondeterministically fail.
        let result = do task::try {
            // a task that has two references to the same exclusive will
            // deadlock when it unwraps. nothing to be done about that.
            let x = exclusive(~~"hello");
            let x2 = x.clone();
            do task::spawn {
                for 10.times { task::yield(); } // try to let the unwrapper go
                fail!(); // punt it awake from its deadlock
            }
            let _z = x.unwrap();
            unsafe { do x2.with |_hello| { } }
        };
        assert!(result.is_err());
    }
}
