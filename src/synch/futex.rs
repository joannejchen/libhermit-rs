use ahash::RandomState;
use core::sync::atomic::{AtomicU32, Ordering::SeqCst};
use hashbrown::{hash_map::Entry, HashMap};

use crate::{
	arch::kernel::{percore::core_scheduler, processor::get_timer_ticks},
	errno::{EAGAIN, EINVAL, ETIMEDOUT},
	scheduler::task::TaskHandlePriorityQueue,
};

use super::spinlock::SpinlockIrqSave;

// TODO: Replace with a concurrent hashmap.
static PARKING_LOT: SpinlockIrqSave<HashMap<usize, TaskHandlePriorityQueue, RandomState>> =
	SpinlockIrqSave::new(HashMap::with_hasher(RandomState::with_seeds(0, 0, 0, 0)));

bitflags! {
	pub struct Flags: u32 {
		/// Use a relative timeout
		const RELATIVE = 0b01;
	}
}

/// If the value at address matches the expected value, park the current thread until it is either
/// woken up with `futex_wake` (returns 0) or the specified timeout elapses (returns -ETIMEDOUT).
///
/// The timeout is given in microseconds. If [`Flags::RELATIVE`] is given, it is interpreted as
/// relative to the current time. Otherwise it is understood to be an absolute time
/// (see `get_timer_ticks`).
pub fn futex_wait(address: &AtomicU32, expected: u32, timeout: Option<u64>, flags: Flags) -> i32 {
	let mut parking_lot = PARKING_LOT.lock();
	// Check the futex value after locking the parking lot so that all changes are observed.
	if address.load(SeqCst) != expected {
		return -EAGAIN;
	}

	let wakeup_time = if flags.contains(Flags::RELATIVE) {
		timeout.and_then(|t| get_timer_ticks().checked_add(t))
	} else {
		timeout
	};

	let scheduler = core_scheduler();
	scheduler.block_current_task(wakeup_time);
	let handle = scheduler.get_current_task_handle();
	parking_lot
		.entry(address.as_mut_ptr().addr())
		.or_default()
		.push(handle);
	drop(parking_lot);

	loop {
		scheduler.reschedule();

		let mut parking_lot = PARKING_LOT.lock();
		if wakeup_time.is_some_and(|t| t <= get_timer_ticks()) {
			let mut wakeup = true;
			// Timeout occurred, try to remove ourselves from the waiting queue.
			if let Entry::Occupied(mut queue) = parking_lot.entry(address.as_mut_ptr().addr()) {
				// If we are not in the waking queue, this must have been a wakeup.
				wakeup = !queue.get_mut().remove(handle);
				if queue.get().is_empty() {
					queue.remove();
				}
			}

			if wakeup {
				return 0;
			} else {
				return -ETIMEDOUT;
			}
		} else {
			// If we are not in the waking queue, this must have been a wakeup.
			let wakeup = !parking_lot
				.get(&address.as_mut_ptr().addr())
				.is_some_and(|queue| queue.contains(handle));

			if wakeup {
				return 0;
			} else {
				// A spurious wakeup occurred, sleep again.
				// Tasks do not change core, so the handle in the parking lot is still current.
				scheduler.block_current_task(wakeup_time);
			}
		}
		drop(parking_lot);
	}
}

/// Wake `count` threads waiting on the futex at address. Returns the number of threads
/// woken up (saturates to `i32::MAX`). If `count` is `i32::MAX`, wake up all matching
/// waiting threads. If `count` is negative, returns -EINVAL.
pub fn futex_wake(address: &AtomicU32, count: i32) -> i32 {
	if count < 0 {
		return -EINVAL;
	}

	let mut parking_lot = PARKING_LOT.lock();
	let mut queue = match parking_lot.entry(address.as_mut_ptr().addr()) {
		Entry::Occupied(entry) => entry,
		Entry::Vacant(_) => return 0,
	};

	let scheduler = core_scheduler();
	let mut woken = 0;
	while woken != count || count == i32::MAX {
		match queue.get_mut().pop() {
			Some(handle) => scheduler.custom_wakeup(handle),
			None => break,
		}
		woken = woken.saturating_add(1);
	}

	if queue.get().is_empty() {
		queue.remove();
	}

	woken
}