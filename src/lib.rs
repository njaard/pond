//! Yet another implementation of a scoped threadpool.
//!
//! A scoped threadpool allows many tasks to be executed
//! in the current function scope, which means that
//! data doesn't need to have a `'static` lifetime.
//!
//! This one is has the additional ability to store a mutable
//! state in each thread, which permits you to, for example, reuse
//! expensive-to-setup database connections, one per thread. Also,
//! you can set a backlog which can prevent an explosion of memory
//! usage if you have many jobs to start.
//!
//! # Example
//!
//! ```rust
//! extern crate pond;
//!
//! fn main()
//! {
//!    // Create a threadpool with the native number of cpus
//!    let mut pool = pond::Pool::new();
//!
//!    let mut vec = vec![0, 0, 0, 0, 0, 0, 0, 0];
//!
//!    // Each thread can access the variables from
//!    // the current scope
//!    pool.scoped(
//!        |scoped|
//!        {
//!            let scoped = scoped.with_state(
//!                || "costly setup function".len()
//!            );
//!            // each thread runs the above setup function
//!
//!            // Create references to each element in the vector ...
//!            for e in &mut vec
//!            {
//!                scoped.execute(
//!                    move |state|
//!                    {
//!                        *e += *state;
//!                        assert_eq!(*e, 21);
//!                    }
//!                );
//!            }
//!        }
//!    );
//!
//!     assert_eq!(vec, vec![21, 21, 21, 21, 21, 21, 21, 21]);
//! }
//! ```
use std::collections::VecDeque;
use std::sync::{Arc,Mutex,Condvar};
use std::thread::JoinHandle;
use std::marker::PhantomData;
use std::any::Any;

extern crate num_cpus;

trait AbstractTask: Send
{
	fn run(self: Box<Self>, state: &mut Box<Any>);
}
struct FnState<State, F>(F, PhantomData<State>)
	where F: FnOnce(&mut State) + Send;
struct FnStateless<F>(F)
	where F: FnOnce() + Send;


impl<State: 'static + Send, F> AbstractTask for FnState<State,F>
	where F: FnOnce(&mut State) + Send
{
	fn run(self: Box<Self>, state: &mut Box<Any>)
	{
		let state_any = &mut *state;
		let state = state_any.downcast_mut().unwrap();
		(*self).0(state);
	}
}

impl<F> AbstractTask for FnStateless<F>
	where F: FnOnce() + Send
{
	fn run(self: Box<Self>, _: &mut Box<Any>)
	{
		(*self).0();
	}
}

trait StateMakerBox
{
	fn create(self: &Self) -> Box<Any>;
}

impl<F: Fn()->Box<Any>> StateMakerBox for F
{
	fn create(self: &F) -> Box<Any>
	{
		(*self)()
	}
}


type StateMaker<'a> = Box<StateMakerBox + Send + 'a>;

#[derive(PartialEq)]
enum Flag
{
	Checkpoint, // all the threads receive this and reply with an ok when the queue is empty
	Resume, // the thread may resume after a checkpoint
	Exit, // all the threads finish
	SetState, // call and store Messaging::state_maker
}

struct Messaging
{
	job_queue : VecDeque<Box<AbstractTask>>,
	flag : Option<Flag>,
	completion_counter : usize,
	panic_detected : bool,
	state_maker : Option<StateMaker<'static>>,
}

impl Messaging
{
	fn new() -> Self
	{
		Messaging
		{
			job_queue: VecDeque::new(),
			flag: None,
			completion_counter: 0,
			panic_detected: false,
			state_maker: None,
		}
	}
}

/// this structure is locked and shared between all the threads
struct PoolStatus
{
	messaging : Mutex<Messaging>,
	incoming_notif_cv : Condvar,
	thread_response_cv : Condvar,
}

/// Holds a number of threads that you can run tasks on
pub struct Pool
{
	threads: Vec<JoinHandle<()>>,
	pool_status : Arc<PoolStatus>,
	backlog: Option<usize>,
}

impl Pool
{
	/// Spawn a pool with the native number of threads and a fixed backlog
	///
	/// The native number of threads is the number of logical cpus
	/// according to the crate [`num_cpus`](https://crates.io/crates/num_cpus)
	///
	/// The backlog, 4 times the number of threads, is the number of
	/// unprocessed jobs that are allowed to accumulate from
	/// [`Scope::execute`](struct.Scope.html#method.execute)
	pub fn new()
		-> Pool
	{
		let c = num_cpus::get();
		Self::base_new(c, Some(c*4))
	}

	/// Spawn a pool with the native number of threads an un unbounded backlog
	///
	/// The native number of threads is the number of logical cpus
	/// according to the crate [`num_cpus`](https://crates.io/crates/num_cpus)
	///
	/// [`Scope::execute`](struct.Scope.html#method.execute) will never block
	/// and will accumulate any task you give it. This may require a lot of memory.
	pub fn unbounded()
		-> Pool
	{
		let c = num_cpus::get();
		Self::base_new(c, None)
	}

	/// Spawn a number of threads. The pool's queue of pending jobs is limited.
	///
	/// `backlog` is the number of jobs that haven't been run.
	/// If specified, [`Scope::execute`](struct.Scope.html#method.execute) will block
	/// until a job completes.
	pub fn new_threads(nthreads : usize, backlog : usize)
		-> Pool
	{
		Self::base_new(nthreads, Some(backlog))
	}

	/// Spawn a number of threads. The pool's queue of pending jobs is limited.
	///
	/// the backlog is unbounded as in `unbounded`.
	pub fn new_threads_unbounded(nthreads : usize)
		-> Pool
	{
		Self::base_new(nthreads, None)
	}

	fn run_thread(pool_status : Arc<PoolStatus>)
	{
		let has_panic = std::panic::catch_unwind(
			std::panic::AssertUnwindSafe(||
			{
				// the default state is one that calls a function
				// with no state parameters

				let mut state = Box::new(0) as Box<Any>;

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
						Some(Flag::SetState) =>
						{
							if !in_checkpoint && messaging.job_queue.is_empty()
							{
								// bug: the state_maker should be called in all
								// threads simultaneously (they all wait on the mutex)
								state = messaging.state_maker.as_ref().unwrap().create();
								messaging.completion_counter += 1;
								in_checkpoint = true;
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
			messaging.panic_detected = true;
		}
		pool_status.thread_response_cv.notify_one();
	}

	fn base_new(nthreads : usize, backlog: Option<usize>)
		-> Pool
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

		for _ in 0..nthreads
		{
			let pool_status = pool_status.clone();

			let t = std::thread::spawn(
				move || Self::run_thread(pool_status)
			);

			threads.push(t);
		}

		Pool
		{
			threads: threads,
			pool_status: pool_status,
			backlog: backlog,
		}
	}

	/// Store the current scope so that you can run jobs in it.
	///
	/// This function panics if the given closure or any threads panic.
	///
	/// Does not return until all executed jobs complete.
	pub fn scoped<'pool, 'scope, F>(&'pool mut self, f : F)
		where F: FnOnce(Scope<'pool, 'scope>)
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
				&& !messaging.panic_detected
			{
				messaging
					= self.pool_status
						.thread_response_cv
						.wait(messaging)
						.unwrap_or_else(|e| e.into_inner());
			}

			if messaging.panic_detected
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

impl Drop for Pool
{
	/// terminates all threads
	fn drop(&mut self)
	{
		{ // tell the threads to stop
			let mut messaging = self.pool_status.messaging
				.lock().unwrap_or_else(|e| e.into_inner());

			if messaging.job_queue.len() != 0
			{
				panic!("pond::Pool: one or more worker thread panicked");
			}
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
pub struct Scope<'pool, 'scope>
{
	pool: &'pool Pool,
	_scope: PhantomData<::std::cell::Cell<&'scope ()>>,
}

impl<'pool, 'scope> Scope<'pool, 'scope>
{
	/// Give each of the threads a state
	///
	/// The parameter is a closure that is run in each thread. The
	/// parameter finishes running before this function exits.
	///
	/// A new scope is returned that allows one to run jobs in the
	/// worker threads which can take the state parameter.
	pub fn with_state<StateMaker, State>(self, state_maker : StateMaker)
		-> ScopeWithState<'pool, 'scope, State>
		where StateMaker: Fn() -> State + Send + 'scope,
			State: 'static
	{
		// `Pool` expects a closure that returns a Box of uncertain type
		let f =
			move ||
			{
				let state = state_maker();
				Box::new(state) as Box<Any>
			};
		let f = unsafe
			{
				std::mem::transmute::<
					Box<StateMakerBox + 'scope + Send>,
					Box<StateMakerBox + 'static + Send>
				>(Box::new(f))
			};

		{ // send a SetState event
			let mut messaging = self.pool.pool_status.messaging
				.lock().unwrap_or_else(|e| e.into_inner());
			messaging.completion_counter = 0;
			messaging.flag = Some(Flag::SetState);
			messaging.state_maker = Some(f);

			self.pool.pool_status.incoming_notif_cv.notify_all();

			// wait for all threads to get the event (or to panic)
			while messaging.completion_counter != self.pool.threads.len()
				&& !messaging.panic_detected
			{
				messaging
					= self.pool.pool_status
						.thread_response_cv
						.wait(messaging)
						.unwrap_or_else(|e| e.into_inner());
			}

			if messaging.panic_detected
			{
				panic!("worker thread panicked");
			}
		}

		{ // tell the threads to resume
			let mut messaging = self.pool.pool_status.messaging
				.lock().unwrap_or_else(|e| e.into_inner());
			messaging.completion_counter = 0;
			messaging.state_maker = None;
			messaging.flag = Some(Flag::Resume);
			self.pool.pool_status.incoming_notif_cv.notify_all();

			// wait for all threads to get the resume event

			while messaging.completion_counter != self.pool.threads.len()
			{
				messaging
					= self.pool.pool_status
						.thread_response_cv
						.wait(messaging)
						.unwrap_or_else(|e| e.into_inner());
			}
			messaging.flag = None;
		}

		ScopeWithState
		{
			pool: self.pool,
			_scope: PhantomData,
			_state: PhantomData,
		}
	}


	/// Execute a job on one of the threads in the threadpool.
	///
	/// The closure is called from one of the threads. If the
	/// threadpool has a specified backlog, then this function
	/// blocks until the threadpool finishes a job.
	///
	/// This function may panic if a job that was previously
	/// `execute`d has panicked. This way, your program terminates
	/// as soon as a panic occurs. If you don't call this function
	/// again after a panic occurs, then [`Pool::scoped`](struct.Pool.html#method.scoped)
	/// will panic before it completes.
	pub fn execute<F>(&self, f: F)
		where F: FnOnce() + Send + 'scope
	{
		// this is where the magic happens,
		// we change the lifetime of `f` and then enforce
		// safety by not returning from the `'scope`
		// until all the `f`s complete.
		let boxed_fn
			= unsafe
			{
				std::mem::transmute::<
					Box<AbstractTask + 'scope>,
					Box<AbstractTask + 'static>
				>(Box::new(FnStateless(f)))
			};

		let mut messaging = self.pool.pool_status.messaging
			.lock().unwrap_or_else(|e| e.into_inner());

		while self.pool.backlog.map(
				|allowed| allowed < messaging.job_queue.len()
			).unwrap_or(false)
		{
			messaging
				= self.pool.pool_status
					.thread_response_cv
					.wait(messaging)
					.unwrap_or_else(|e| e.into_inner());
		}

		messaging.job_queue.push_back( boxed_fn );

		if messaging.panic_detected
		{
			panic!("worker thread panicked");
		}

		self.pool.pool_status.incoming_notif_cv.notify_one();
	}
}


/// Like `Scope`, but the `execute` function
/// accepts worker closures with a State parameter.
pub struct ScopeWithState<'pool, 'scope, State>
	where State : 'static
{
	pool: &'pool Pool,
	_scope: PhantomData<::std::cell::Cell<&'scope ()>>,
	_state: PhantomData<&'scope State>,
}

impl<'pool, 'scope, State> ScopeWithState<'pool, 'scope, State>
	where State: 'static + Send
{
	/// Execute a job on one of the threads in the threadpool.
	///
	/// The closure is called from one of the threads. If the
	/// threadpool has a specified backlog, then this function
	/// blocks until the threadpool finishes a job.
	///
	/// The closure is passed a mutable reference to the
	/// the state produced by the function passed to [`Scope::with_state`](struct.Scope.html#method.with_state)
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
					Box<AbstractTask + 'scope>,
					Box<AbstractTask + 'static>
				>(Box::new(FnState(f, PhantomData)))
			};

		let mut messaging = self.pool.pool_status.messaging
			.lock().unwrap_or_else(|e| e.into_inner());

		while self.pool.backlog.map(
				|allowed| allowed < messaging.job_queue.len()
			).unwrap_or(false)
		{
			messaging
				= self.pool.pool_status
					.thread_response_cv
					.wait(messaging)
					.unwrap_or_else(|e| e.into_inner());
		}

		messaging.job_queue.push_back( boxed_fn );

		if messaging.panic_detected
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
		let mut pool = Pool::new_threads_unbounded(4);

		for i in 1..7
		{
			let mut vec = vec![0, 1, 2, 3, 4];
			pool.scoped(
				|s|
				{
					for e in vec.iter_mut()
					{
						s.execute(
							move ||
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

	// this one should not compile
	/*
	#[test]
	fn negative_test()
	{
		let mut pool = Pool::new_threads_unbounded(60);

		let all = ::std::sync::Mutex::new(vec!());

		pool.scoped(
			|s|
			for i in 0..10
			{
				s.execute(
					||
					all.lock().unwrap().push(i)
				);
			}
		);

		let mut all = all.into_inner().unwrap();
		all.sort();

		assert_eq!(all, [0,1,2,3,4,5,6,7,8,9]);
	}
	*/

	#[test]
	#[should_panic]
	fn thread_panic()
	{
		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				scoped.execute(
					move ||
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
		let mut pool = Pool::new_threads_unbounded(4);
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
		let _pool = Pool::new_threads_unbounded(4);
		panic!()
	}

	#[test]
	fn join_all()
	{
		let mut pool = Pool::new_threads_unbounded(4);

		let (tx_, rx) = sync::mpsc::channel();

		pool.scoped(
			|scoped|
			{
				let tx = tx_.clone();
				scoped.execute(
					move ||
					{
						sleep_ms(1000);
						tx.send(2).unwrap();
					}
				);

				let tx = tx_.clone();
				scoped.execute(
					move ||
					{
						tx.send(1).unwrap();
					}
				);

				let tx = tx_.clone();
				scoped.execute(
					move ||
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
				let mut pool = Pool::new_threads_unbounded(8);
				let _on_scope_end = OnScopeEnd(tx_.clone());
				pool.scoped(
					|scoped|
					{
						scoped.execute(
							move ||
							{
								sleep_ms(1000);
								panic!();
							}
						);
						for _ in 1..8
						{
							let tx = tx_.clone();
							scoped.execute(
								move ||
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

		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				let c = ::std::sync::Arc::clone(&counters);
				scoped.execute(
					move ||
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
	fn no_leak2()
	{
		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				for _ in 0..4
				{
					scoped.execute(
						move ||
						{
							sleep_ms(100);
						}
					);
				}
			}
		);
	}

	#[test]
	fn no_leak_state()
	{
		let counters = ::std::sync::Arc::new(());

		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				scoped.execute(
					||
					{
						let _c = ::std::sync::Arc::clone(&counters);
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
		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				scoped.execute(
					move ||
					{
					}
				);
			}
		);
	}

	#[test]
	fn backlog_positive()
	{
		let mut pool = Pool::new_threads(4, 1);
		pool.scoped(
			|scoped|
			{
				let begin = ::std::time::Instant::now();
				for _ in 0..16
				{
					scoped.execute(
						move ||
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
		let mut pool = Pool::new_threads_unbounded(20);
		pool.scoped(
			|scoped|
			{
				let begin = ::std::time::Instant::now();
				for _ in 0..120
				{
					scoped.execute(
						move ||
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
		let mut pool = Pool::new_threads_unbounded(40);
		pool.scoped(
			|scoped|
			{
				for _ in 0..120
				{
					scoped.execute(
						move ||
						{
							sleep_ms(200);
						}
					);
				}
			}
		);
	}

	#[test]
	fn state_creator_scope()
	{
		let sc = "hello".to_string();

		let counter = ::std::sync::Mutex::new(0);

		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				let scoped = scoped.with_state(
					||
					{
						*counter.lock().unwrap() += 1;
						sc.clone()
					}
				);

				for _ in 0..120
				{
					scoped.execute(
						move |_|
						{
						}
					);
				}
			}
		);

		assert_eq!(*counter.lock().unwrap(), 4);
	}

	#[test]
	fn modify_state()
	{
		let counter = ::std::sync::Arc::new(::std::sync::Mutex::new(0));

		let mut pool = Pool::new_threads_unbounded(4);
		pool.scoped(
			|scoped|
			{
				let scoped = scoped.with_state(
					||
					{
						counter.clone()
					}
				);

				for _ in 0..120
				{
					scoped.execute(
						move |f|
						{
							*f.lock().unwrap() += 1;
						}
					);
				}
			}
		);

		assert_eq!(*counter.lock().unwrap(), 120);
	}

	#[test]
	fn do_them_all()
	{
		let counter = ::std::sync::Arc::new(::std::sync::Mutex::new(0));

		let mut pool = Pool::new_threads(512, 1000);
		pool.scoped(
			|scoped|
			{
				for _ in 0..3000
				{
					let counter = counter.clone();
					scoped.execute(
						move ||
						{
							*counter.lock().unwrap() += 1;
							sleep_ms(1000);
						}
					);
				}
			}
		);

		assert_eq!(*counter.lock().unwrap(), 3000);
	}

	#[test]
	fn state_maker_panic()
	{
		let mut pool = Pool::new_threads_unbounded(2);
		let panic = ::std::panic::catch_unwind(
			::std::panic::AssertUnwindSafe(
				||
				{
					pool.scoped(
						|scoped|
						{
							scoped.with_state(
								|| panic!()
							);
						}
					);
				}
			)
		);
		if let Err(e) = panic
		{
			let s = e.downcast_ref::<&str>().unwrap();
			assert_eq!(
				*s,
				"worker thread panicked"
			);
		}
	}
}
