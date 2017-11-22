[![GitHub license](https://img.shields.io/badge/license-BSD-blue.svg)](https://raw.githubusercontent.com/njaard/scope-threadpool/master/LICENSE)
[![Crates.io](https://img.shields.io/crates/v/scope-threadpool.svg)](https://crates.io/crates/scope-threadpool)
[![Documentation](https://docs.rs/scope-threadpool/badge.svg)](https://docs.rs/scope-threadpool/)

	[dependencies]
	scope-threadpool = "0.1"

# Introduction
Yet another implementation of a scoped threadpool.

This one is has the additional ability to store a mutable
state in each thread, which permits you to, for example, reuse
expensive to setup database connections, one per thread. Also,
you can set a backlog which can prevent an explosion of memory
usage if you have many jobs to start.

A scoped threadpool allows many tasks to be executed
in the current function scope, which means that
data doesn't need to have a `'static` lifetime.

# Usecase

If you need to make multiple connections to a database server,
without this crate, you need some sort of connection pooling library,
and therefor each connection needs Rust's `Send` capability. Furthermore,
there's no guarantee that your connection pooler will keep the
connection on the same thread.

With this crate, you provide a function that sets up the connection,
then the function is called in each thread at initialization time.
A mutable reference is passed to your job closures. It's your
job's responsibility to make sure that each job keep the database
connection in a sane state between jobs.

# Example

    let mut pool = scope_threadpool::Pool::new(4);


    let mut vec = vec![0, 0, 0, 0, 0, 0, 0, 0];

    // Each thread can access the variables from
    // the current scope
    pool.scoped(
        |scoped|
        {
            let scoped = scoped.with_state(|| "costly setup function".len());
            // each thread gets its own mutable copy of the result of
            // the above setup function

            // Create references to each element in the vector ...
            for e in &mut vec
            {
                scoped.execute(
                    move |state|
                    {
                        *e += *state;
                    }
                );
            }
        }
    );


# See Also

* [scoped-threadpool](https://crates.io/crates/scoped-threadpool) (Has a very similar API, but no state). Please beware [a serious bug](https://github.com/Kimundi/scoped-threadpool-rs/issues/16).
* [scoped_pool](https://crates.io/crates/scoped_pool) (Very flexible, but no state)
* [crossbeam](https://crates.io/crates/crossbeam) (doesn't implement a thread pool)

