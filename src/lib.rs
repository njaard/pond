//! Yet another implementation of a scoped threadpool.
//!
//! This one is has the additional ability to store a mutable
//! state in each thread, which permits you to, for example, reuse
//! expensive to setup database connections, one per thread. Also,
//! you can set a backlog which can prevent an explosion of memory
//! usage if you have many jobs to start.
//!
//! A scoped threadpool allows many tasks to be executed
//! in the current function scope, which means that
//! data doesn't need to have a `'static` lifetime.
//!
//! # Example
//!
//! ```rust
//! extern crate scope_threadpool;
//!
//! fn main()
//! {
//!     // Create a threadpool holding 4 threads.
//!     // each thread creates its state from the given function
//!     let mut pool = scope_threadpool::Pool::new(
//!         4,
//!         || "costly setup function".len()
//!     );
//!
//!     // each thread gets its own mutable copy of the result of
//!     // the above setup function
//!
//!     let mut vec = vec![0, 0, 0, 0, 0, 0, 0, 0];
//!
//!     // Each thread can access the variables from
//!     // the current scope
//!     pool.scoped(
//!         |scoped|
//!         {
//!             // Create references to each element in the vector ...
//!             for e in &mut vec
//!             {
//!                 scoped.execute(
//!                     move |state|
//!                     {
//!                         *e += *state;
//!                     }
//!                 );
//!             }
//!         }
//!     );
//!
//!     assert_eq!(vec, vec![21, 21, 21, 21, 21, 21, 21, 21]);
//! }
//! ```

use std::collections::VecDeque;
use std::sync::{Arc,Mutex,Condvar};
use std::thread::JoinHandle;
use std::marker::PhantomData;

trait Task<State>
{
	fn run(self: Box<Self>, &mut State);
}

impl<State, F: FnOnce(&mut State)> Task<State> for F
{
	fn run(self: Box<Self>, state : &mut State) { (*self)(state); }
}

#[derive(PartialEq)]
enum Flag
{
	Checkpoint, // all the threads receive this and reply with an ok when the queue is empty
	Resume, // the thread may resume after a checkpoint
	Exit, // all the threads finish
}

struct Messaging<State>
{
	job_queue : VecDeque<Box<Task<State> + Send>>,
	flag : Option<Flag>,
	completion_counter : usize,
	panic_counter : usize,
}

impl<State> Messaging<State>
{
	fn new() -> Self
	{
		Messaging
		{
			job_queue: VecDeque::new(),
			flag: None,
			completion_counter: 0,
			panic_counter: 0,
		}
	}
}

/// this structure is locked and shared between all the threads
struct PoolStatus<State>
{
	messaging : Mutex<Messaging<State>>,
	incoming_notif_cv : Condvar,
	thread_response_cv : Condvar,
}

/// Holds a number of threads that you can run tasks on
pub struct Pool<StateMaker, State>
{
	threads: Vec<JoinHandle<()>>,
	pool_status : Arc<PoolStatus<State>>,
	_state: PhantomData<StateMaker>,
	backlog: Option<usize>,
}

impl<StateMaker, State> Pool<StateMaker, State>
	where StateMaker: Fn() -> State + 'static + Send + Sync,
	State : 'static
{
	/// Spawn a number of threads each of which call a state making function.
	pub fn new(nthreads : usize, state_maker : StateMaker)
		-> Pool<StateMaker, State>
	{
		Self::base_new(nthreads, None, state_maker)
	}

	/// Spawn a number of threads. The pool's queue of pending jobs is limited.
	///
	/// `backlog` is the number of jobs that haven't been run.
	/// If specified, [`Scope::execute`](struct.Scope.html#method.execute) will block
	/// until a job completes.
	///
	/// `state_maker` is a function that is run on each thread and creates
	/// an object, owned by that thread, which is passed to the jobs' closures.
	pub fn new_with_backlog(nthreads : usize, backlog : usize, state_maker : StateMaker)
		-> Pool<StateMaker, State>
	{
		Self::base_new(nthreads, Some(backlog), state_maker)
	}

	fn base_new(nthreads : usize, backlog: Option<usize>, state_maker : StateMaker)
		-> Pool<StateMaker, State>
	{
		assert!(nthreads >= 1);

		let mut threads = Vec::with_capacity(nthreads);

		let pool_status = Arc::new(
			PoolStatus
			{
				messaging: Mutex::new(Messaging::new()),
				incoming_notif_cv : Condvar::new(),
				thread_response_cv : Condvar::new(),
			}
		);

		let state_maker = Arc::new(state_maker);

		for _ in 0..nthreads
		{
			let pool_status = pool_status.clone();
			let state_maker = state_maker.clone();

			let t = std::thread::spawn(
				move ||
				{
					let has_panic = std::panic::catch_unwind(
						std::panic::AssertUnwindSafe(||
						{
							let mut state = state_maker();
							let messaging = &pool_status.messaging;

							let mut in_checkpoint = false;

							loop
							{
								let mut messaging = messaging
									.lock()
									.unwrap_or_else(|e| e.into_inner());

								while (
									in_checkpoint
										&& *messaging.flag.as_ref()
											.unwrap_or(&Flag::Checkpoint)
											== Flag::Checkpoint
									) || (
										messaging.job_queue.is_empty()
										&& messaging.flag.is_none()
									)
								{
									messaging = pool_status
										.incoming_notif_cv
										.wait(messaging)
										.unwrap_or_else(|e| e.into_inner());
								}

								match messaging.flag
								{
									Some(Flag::Checkpoint) =>
									{
										if !in_checkpoint && messaging.job_queue.is_empty()
										{
											messaging.completion_counter += 1;
											in_checkpoint = true;
											pool_status.thread_response_cv.notify_one();
											continue;
										}
									},
									Some(Flag::Resume) =>
									{
										if in_checkpoint
										{
											messaging.completion_counter += 1;
											in_checkpoint = false;
											pool_status.thread_response_cv.notify_one();
										}
									},
									Some(Flag::Exit) =>
									{
										return;
									},
									None => { }
								}

								if let Some(t) = messaging.job_queue.pop_front()
								{
									pool_status.thread_response_cv.notify_one();
									drop(messaging);
									t.run(&mut state);
								}
							}
						})
					);

					let mut messaging = pool_status.messaging
						.lock().unwrap_or_else(|e| e.into_inner());
					if has_panic.is_err()
					{
						messaging.panic_counter += 1;
					}
					pool_status.thread_response_cv.notify_one();

				}
			);

			threads.push(t);
		}


		Pool
		{
			threads: threads,
			pool_status: pool_status,
			_state : PhantomData,
			backlog: backlog,
		}
	}

	/// Store the current scope so that you can run jobs in it.
	///
	/// This function panics if the given closure or any threads panic.
	///
	/// Does not return until all executed jobs complete.
	pub fn scoped<F>(&mut self, f : F)
		where F: FnOnce(Scope<StateMaker, State>)
	{
		{
			let scope = Scope
			{
				pool: self,
				_scope: PhantomData,
			};

			f(scope);
		}

		{ // send a checkpoint event
			let mut messaging = self.pool_status.messaging
				.lock().unwrap_or_else(|e| e.into_inner());
			messaging.completion_counter = 0;

			messaging.flag = Some(Flag::Checkpoint);
			self.pool_status.incoming_notif_cv.notify_all();

			// wait for all threads to get the checkpoint event (or to panic)
			while messaging.completion_counter != self.threads.len()
				&& 0==messaging.panic_counter
			{
				messaging
					= self.pool_status
						.thread_response_cv
						.wait(messaging)
						.unwrap_or_else(|e| e.into_inner());
			}

			if messaging.panic_counter != 0
			{
				panic!("worker thread panicked");
			}
		}

		{ // tell the threads to resume
			let mut messaging = self.pool_status.messaging
				.lock().unwrap_or_else(|e| e.into_inner());
			messaging.completion_counter = 0;

			messaging.flag = Some(Flag::Resume);
			self.pool_status.incoming_notif_cv.notify_all();

			// wait for all threads to get the resume event

			while messaging.completion_counter != self.threads.len()
			{
				messaging
					= self.pool_status
						.thread_response_cv
						.wait(messaging)
						.unwrap_or_else(|e| e.into_inner());
			}
			//writeln!(&mut std::io::stderr(), "resume ctr is {}, finished!!!", messaging.completion_counter).unwrap();
			messaging.flag = None;
		}
	}
}

impl<StateMaker, State> Drop for Pool<StateMaker, State>
{
	/// terminates all threads
	fn drop(&mut self)
	{
		{ // tell the threads to stop
			let mut messaging = self.pool_status.messaging
				.lock().unwrap_or_else(|e| e.into_inner());

			messaging.flag = Some(Flag::Exit);
			self.pool_status.incoming_notif_cv.notify_all();
		}
		for t in self.threads.drain(..)
		{
			t.join().unwrap();
		}
	}
}

/// Represents the current scope, you can execute
/// functions in it.
pub struct Scope<'pool, 'scope, StateMaker, State>
	where StateMaker: Fn() -> State + 'static + Send + Sync,
	State : 'static
{
	pool: &'pool mut Pool<StateMaker, State>,
	_scope: PhantomData<&'scope ()>,
}

impl<'pool, 'scope, StateMaker, State> Scope<'pool, 'scope, StateMaker, State>
	where StateMaker: Fn() -> State + Send + Sync
{
	/// Execute a job on one of the threads in the threadpool.
	///
	/// The closure is called from one of the threads. If the
	/// threadpool has a specified backlog, then this function
	/// blocks until the threadpool finishes a job.
	///
	/// The closure is passed a mutable reference to the
	/// the state produced by the function passed to [`Pool::new`](struct.Pool.html#method.new)
	///
	/// This function may panic if a job that was previously
	/// `execute`d has panicked. This way, your program terminates
	/// as soon as a panic occurs. If you don't call this function
	/// again after a panic occurs, then [`Pool::scoped`](struct.Pool.html#method.scoped)
	/// will panic before it completes.
	pub fn execute<F>(&self, f: F)
		where F: FnOnce(&mut State) + Send + 'scope
	{
		// this is where the magic happens,
		// we change the lifetime of `f` and then enforce
		// safety by not returning from the `'scope`
		// until all the `f`s complete.
		let boxed_fn
			= unsafe
			{
				std::mem::transmute::<
					Box<Task<State> + Send + 'scope>,
					Box<Task<State> + Send + 'static>
				>(Box::new(f))
			};

		let mut messaging = self.pool.pool_status.messaging
			.lock().unwrap_or_else(|e| e.into_inner());

		while self.pool.backlog.map(|allowed| allowed < messaging.job_queue.len()).unwrap_or(false)
		{
			messaging
				= self.pool.pool_status
					.thread_response_cv
					.wait(messaging)
					.unwrap_or_else(|e| e.into_inner());
		}

		messaging.job_queue.push_back( boxed_fn );

		if messaging.panic_counter != 0
		{
			panic!("worker thread panicked");
		}

		self.pool.pool_status.incoming_notif_cv.notify_one();
	}
}


// Many of these tests come from the crate scoped-threadpool
// <https://github.com/Kimundi/scoped-threadpool-rs>, by Marvin Löbel
#[cfg(test)]
mod tests
{
	use super::Pool;
	use std::thread;
	use std::sync;
	use std::time;

	fn sleep_ms(ms: u64)
	{
		thread::sleep(time::Duration::from_millis(ms));
	}
	#[test]
	fn smoketest()
	{
		let mut pool = Pool::new(4, &|| ());

		for i in 1..7
		{
			let mut vec = vec![0, 1, 2, 3, 4];
			pool.scoped(
				|s|
				{
					for e in vec.iter_mut()
					{
						s.execute(
							move |_|
							{
								*e += i;
							}
						);
					}
				}
			);

			let mut vec2 = vec![0, 1, 2, 3, 4];
			for e in vec2.iter_mut()
			{
				*e += i;
			}

			assert_eq!(vec, vec2);
		}
	}

	#[test]
	#[should_panic]
	fn thread_panic()
	{
		let mut pool = Pool::new(4, &|| ());
		pool.scoped(
			|scoped|
			{
				scoped.execute(
					move |_|
					{
						panic!()
					}
				);
			}
		);
	}

	#[test]
	#[should_panic]
	fn scope_panic()
	{
		let mut pool = Pool::new(4, &|| ());
		pool.scoped(
			|_scoped|
			{
				panic!()
			}
		);
	}

	#[test]
	#[should_panic]
	fn pool_panic()
	{
		let _pool = Pool::new(4, &|| ());
		panic!()
	}

	#[test]
	fn join_all()
	{
		let mut pool = Pool::new(4, &|| ());

		let (tx_, rx) = sync::mpsc::channel();

		pool.scoped(
			|scoped|
			{
				let tx = tx_.clone();
				scoped.execute(
					move |_|
					{
						sleep_ms(1000);
						tx.send(2).unwrap();
					}
				);

				let tx = tx_.clone();
				scoped.execute(
					move |_|
					{
						tx.send(1).unwrap();
					}
				);

				let tx = tx_.clone();
				scoped.execute(
					move |_|
					{
						sleep_ms(500);
						tx.send(3).unwrap();
					}
				);
			}
		);

		assert_eq!(rx.iter().take(3).collect::<Vec<_>>(), vec![1, 3, 2]);
	}

	#[test]
	fn join_all_with_thread_panic()
	{
		use std::sync::mpsc::Sender;
		struct OnScopeEnd(Sender<u8>);
		impl Drop for OnScopeEnd
		{
			fn drop(&mut self)
			{
				self.0.send(1).unwrap();
				sleep_ms(200);
			}
		}
		let (tx_, rx) = sync::mpsc::channel();
		// Use a thread here to handle the expected panic from the pool. Should
		// be switched to use panic::recover instead when it becomes stable.
		let handle = thread::spawn(
			move ||
			{
				let mut pool = Pool::new(8, &|| ());
				let _on_scope_end = OnScopeEnd(tx_.clone());
				pool.scoped(
					|scoped|
					{
						scoped.execute(
							move |_|
							{
								sleep_ms(1000);
								panic!();
							}
						);
						for _ in 1..8
						{
							let tx = tx_.clone();
							scoped.execute(
								move |_|
								{
									sleep_ms(2000);
									tx.send(0).unwrap();
								}
							);
						}
					}
				);
			}
		);
		if let Ok(..) = handle.join()
		{
			panic!("Pool didn't panic as expected");
		}
		// If the `1` that OnScopeEnd sent occurs anywhere else than at the
		// end, that means that a worker thread was still running even
		// after the `scoped` call finished, which is unsound.
		let values: Vec<u8> = rx.into_iter().collect();
		assert_eq!(&values[..], &[1, 0, 0, 0, 0, 0, 0, 0]);
	}

	#[test]
	fn no_leak()
	{
		let counters = ::std::sync::Arc::new(());

		let mut pool = Pool::new(4, || ());
		pool.scoped(
			|scoped|
			{
				let c = ::std::sync::Arc::clone(&counters);
				scoped.execute(
					move |_|
					{
						let _c = c;
						sleep_ms(100);
					}
				);
			}
		);
		drop(pool);
		assert_eq!(::std::sync::Arc::strong_count(&counters), 1);
	}

	#[test]
	fn no_leak_state()
	{
		let counters = ::std::sync::Arc::new(());
		let cclone = counters.clone();

		let mut pool = Pool::new(4, move || cclone.clone());
		pool.scoped(
			|scoped|
			{
				scoped.execute(
					move |c|
					{
						let _c = ::std::sync::Arc::clone(&c);
					}
				);
			}
		);
		drop(pool);
		assert_eq!(::std::sync::Arc::strong_count(&counters), 1);
	}

	#[test]
	fn safe_execute()
	{
		let mut pool = Pool::new(4, &|| ());
		pool.scoped(
			|scoped|
			{
				scoped.execute(
					move |_|
					{
					}
				);
			}
		);
	}

	#[test]
	fn backlog_positive()
	{
		let mut pool = Pool::new_with_backlog(4, 1, &|| ());
		pool.scoped(
			|scoped|
			{
				let begin = ::std::time::Instant::now();
				for _ in 0..16
				{
					scoped.execute(
						move |_|
						{
							sleep_ms(1000);
						}
					);
				}
				assert!(::std::time::Instant::now().duration_since(begin).as_secs() > 1);
			}
		);
	}

	#[test]
	fn backlog_negative()
	{
		let mut pool = Pool::new(20, &|| ());
		pool.scoped(
			|scoped|
			{
				let begin = ::std::time::Instant::now();
				for _ in 0..120
				{
					scoped.execute(
						move |_|
						{
							sleep_ms(1000);
						}
					);
				}
				assert!(::std::time::Instant::now().duration_since(begin).as_secs() < 2);
			}
		);
	}

	#[test]
	fn many_threads()
	{
		let mut pool = Pool::new(40, &|| ());
		pool.scoped(
			|scoped|
			{
				for _ in 0..120
				{
					scoped.execute(
						move |_|
						{
							sleep_ms(200);
						}
					);
				}
			}
		);
	}
}
