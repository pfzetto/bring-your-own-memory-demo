#![no_std]

use core::cell::RefCell;
use core::future::{poll_fn, Future};
use core::mem::MaybeUninit;
use core::task::{Poll, Waker};

use core::ptr::NonNull;

use critical_section::Mutex;
use heapless::Deque;

#[derive(PartialEq)]
pub enum Error {
    Overflowed,
}

pub type CanData = [u8; 8];
pub type CanId = u32;

pub struct CanDriver {
    list: Mutex<RefCell<Option<CanDriverClientList>>>,
}

struct CanDriverClientList {
    head: NonNull<CanDriverClient>,
    tail: NonNull<CanDriverClient>,
}

struct CanDriverClient {
    receiver: NonNull<dyn Receive>,
    nav: Mutex<RefCell<CanDriverClientNav>>,
}

struct CanDriverClientNav {
    next: Option<NonNull<CanDriverClient>>,
    prev: Option<NonNull<CanDriverClient>>,
}

struct Receiver<'candriver, const N: usize> {
    can_driver: &'candriver CanDriver,
    client: Option<CanDriverClient>,
    inner: Mutex<RefCell<ReceiverInner<N>>>,
}
struct ReceiverInner<const N: usize> {
    queue: Deque<(CanId, CanData), N>,
    overflowed: bool,
    waker: Option<Waker>,
}

trait Receive {
    fn receive(&self, value: &(CanId, CanData));
}

pub struct UnboundReceiver<'candriver, const N: usize>(MaybeUninit<Receiver<'candriver, N>>);
pub struct ReceiverRef<'receiver, 'candriver: 'receiver, const N: usize>(
    &'receiver mut Receiver<'candriver, N>,
);

impl CanDriver {
    pub fn register<'candriver, 'receiver, const N: usize>(
        &'candriver self,
        memory: &'receiver mut UnboundReceiver<'candriver, N>,
    ) -> ReceiverRef<'receiver, 'candriver, N>
    where
        'candriver: 'receiver,
    {
        let receiver = memory.0.write(Receiver {
            can_driver: self,
            client: None,
            inner: Mutex::new(RefCell::new(ReceiverInner {
                queue: Deque::new(),
                overflowed: false,
                waker: None,
            })),
        });

        receiver.client = Some(CanDriverClient {
            receiver: NonNull::new(receiver as *mut _ as *const dyn Receive as *mut _).unwrap(),
            nav: Mutex::new(RefCell::new(CanDriverClientNav {
                next: None,
                prev: None,
            })),
        });
        let Some(client_ref): &Option<CanDriverClient> = &receiver.client else {
            unreachable!()
        };
        let client_ptr = NonNull::new(client_ref as *const _ as *mut _).unwrap();

        critical_section::with(|cs| {
            let mut client_list = self.list.borrow(cs).borrow_mut();
            if let Some(client_list) = &mut *client_list {
                let old_head = client_list.head;
                client_ref.nav.borrow(cs).borrow_mut().next = Some(old_head);

                let old_head = unsafe { old_head.as_ref() };
                old_head.nav.borrow(cs).borrow_mut().prev = Some(client_ptr);
                client_list.head = client_ptr;
            } else {
                *client_list = Some(CanDriverClientList {
                    head: client_ptr,
                    tail: client_ptr,
                });
            }
        });

        ReceiverRef(receiver)
    }
    pub fn distribute(&self, value: &(CanId, CanData)) {
        critical_section::with(|cs| {
            let list = self.list.borrow(cs).borrow();
            if let Some(list) = &*list {
                let mut next = Some(list.head);
                while let Some(current) = next {
                    let current = unsafe { current.as_ref() };

                    let receiver = unsafe { current.receiver.as_ref() };
                    receiver.receive(value);

                    next = current.nav.borrow(cs).borrow().next;
                }
            }
        })
    }
}

impl<'receiver, 'candriver: 'receiver, const N: usize> ReceiverRef<'receiver, 'candriver, N> {
    pub fn recv(&mut self) -> impl Future<Output = Result<(CanId, CanData), Error>> + '_ {
        poll_fn(|cx| {
            match critical_section::with(|cs| {
                let mut inner = self.0.inner.borrow(cs).borrow_mut();

                // register waker
                let waker = cx.waker();
                if !inner
                    .waker
                    .as_ref()
                    .map(|x| x.will_wake(waker))
                    .unwrap_or_default()
                {
                    let mut waker = Some(waker.clone());
                    core::mem::swap(&mut inner.waker, &mut waker);
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }

                if inner.overflowed {
                    inner.overflowed = false;
                    Err(Error::Overflowed)
                } else {
                    Ok(inner.queue.pop_back())
                }
            }) {
                Err(x) => Poll::Ready(Err(x)),
                Ok(Some(x)) => Poll::Ready(Ok(x)),
                Ok(None) => Poll::Pending,
            }
        })
    }
    pub fn try_recv(&mut self) -> Result<Option<(CanId, CanData)>, Error> {
        critical_section::with(|cs| {
            let mut inner = self.0.inner.borrow(cs).borrow_mut();

            if inner.overflowed {
                inner.overflowed = false;
                Err(Error::Overflowed)
            } else {
                Ok(inner.queue.pop_back())
            }
        })
    }
}

impl<'candriver, const N: usize> Receive for Receiver<'candriver, N> {
    fn receive(&self, value: &(CanId, CanData)) {
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            if inner.queue.is_full() {
                inner.queue.pop_back();
                inner.overflowed = true;
            }
            inner.queue.push_front(value.clone()).unwrap();
        })
    }
}

impl<'candriver, const N: usize> Default for UnboundReceiver<'candriver, N> {
    fn default() -> Self {
        Self(MaybeUninit::uninit())
    }
}

impl Default for CanDriver {
    fn default() -> Self {
        Self {
            list: Mutex::new(RefCell::new(None)),
        }
    }
}

impl<'receiver, 'candriver, const N: usize> Drop for ReceiverRef<'receiver, 'candriver, N> {
    fn drop(&mut self) {
        critical_section::with(|cs| {
            let receiver = &mut self.0;
            let own_client = receiver.client.as_mut().unwrap();
            let own_nav = own_client.nav.borrow(cs).borrow_mut();

            match (own_nav.prev, own_nav.next) {
                (None, None) => {
                    // this item is the last one in the list, we need to tear down the entire list
                    let mut can_driver_list = receiver.can_driver.list.borrow(cs).borrow_mut();
                    *can_driver_list = None;
                }
                (None, Some(next)) => {
                    // this item is the first in the list, we need to update the head ptr and the
                    // prev ptr of next
                    let mut can_driver_list = receiver.can_driver.list.borrow(cs).borrow_mut();
                    let can_driver_list = can_driver_list.as_mut().unwrap();
                    can_driver_list.head = next;

                    let next = unsafe { next.as_ref() };
                    next.nav.borrow(cs).borrow_mut().prev = None;
                }
                (Some(prev), None) => {
                    // this item is the last one in the list, we need to update the tail ptr and
                    // the next ptr of prev

                    let mut can_driver_list = receiver.can_driver.list.borrow(cs).borrow_mut();
                    let can_driver_list = can_driver_list.as_mut().unwrap();
                    can_driver_list.tail = prev;

                    let prev = unsafe { prev.as_ref() };
                    prev.nav.borrow(cs).borrow_mut().next = None;
                }
                (Some(prev), Some(next)) => {
                    // this item is in the middle of the list, we need to update the next ptr of
                    // prev and the prev ptr of next

                    {
                        let next = unsafe { next.as_ref() };
                        next.nav.borrow(cs).borrow_mut().prev = Some(prev);
                    }
                    {
                        let prev = unsafe { prev.as_ref() };
                        prev.nav.borrow(cs).borrow_mut().next = Some(next);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod test {
    use crate::{CanDriver, Error, UnboundReceiver};

    #[test]
    pub fn overflow() {
        let can_driver = CanDriver::default();

        let mut recv1 = UnboundReceiver::default();
        let mut recv1 = can_driver.register::<10>(&mut recv1);

        //test overflowing behaviour
        assert!(recv1.try_recv() == Ok(None));
        for i in 0..11 {
            let msg = (i as u32, [i as u8; 8]);
            can_driver.distribute(&msg);
        }
        assert!(recv1.try_recv() == Err(Error::Overflowed));
        for i in 1..11 {
            let msg = (i as u32, [i as u8; 8]);
            assert!(recv1.try_recv() == Ok(Some(msg)));
        }
        assert!(recv1.try_recv() == Ok(None));
    }
    #[test]
    pub fn multi_receiver() {
        let can_driver = CanDriver::default();

        let mut recv1 = UnboundReceiver::default();
        let mut recv1 = can_driver.register::<2>(&mut recv1);

        let mut recv2 = UnboundReceiver::default();
        let mut recv2 = can_driver.register::<2>(&mut recv2);

        {
            let msg = (0, [0; 8]);
            can_driver.distribute(&msg);
            assert!(recv1.try_recv() == Ok(Some(msg)));
            assert!(recv2.try_recv() == Ok(Some(msg)));
        }

        {
            let msg0 = (1, [0; 8]);
            let msg1 = (2, [0; 8]);
            can_driver.distribute(&msg0);
            can_driver.distribute(&msg1);
            assert!(recv1.try_recv() == Ok(Some(msg0)));
            assert!(recv1.try_recv() == Ok(Some(msg1)));
            assert!(recv2.try_recv() == Ok(Some(msg0)));
            assert!(recv2.try_recv() == Ok(Some(msg1)));
        }

        drop(recv1);

        {
            let msg = (3, [0; 8]);
            can_driver.distribute(&msg);
            assert!(recv2.try_recv() == Ok(Some(msg)));
        }
    }
}

fn main() {
    let can_driver = CanDriver::default();
    spawn(handler_one(&can_driver));
    spawn(handler_two(&can_driver));

    todo!("start executor")
}

/// spawns the future using a executor like RTIC or SMOL
fn spawn<F: Future<Output = ()>>(f: F) {
    todo!()
}

async fn handler_one(can_driver: &CanDriver) {
    let mut can_receiver = UnboundReceiver::default();
    let mut can_receiver = can_driver.register::<10>(&mut can_receiver);

    loop {
        let msg = can_receiver.recv().await;

        todo!("do something");
    }
}

async fn handler_two(can_driver: &CanDriver) {
    let mut can_receiver = UnboundReceiver::default();
    let mut can_receiver = can_driver.register::<1>(&mut can_receiver);

    loop {
        let msg = can_receiver.recv().await;

        todo!("do something");
    }
}
