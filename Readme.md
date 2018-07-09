[![GitHub license](https://img.shields.io/badge/license-BSD-blue.svg)](https://raw.githubusercontent.com/njaard/pond/master/LICENSE)
[![Crates.io](https://img.shields.io/crates/v/pond.svg)](https://crates.io/crates/pond)
[![Documentation](https://docs.rs/pond/badge.svg)](https://docs.rs/pond/)

	[dependencies]
	pond = "0.3"

# Introduction
Yet another implementation of a scoped threadpool.

A scoped threadpool allows many tasks to be executed
in the current function scope, which means that
data doesn't need to have a `'static` lifetime.

This one is has the additional ability to store a mutable
state in each thread, which permits you to, for example, reuse
expensive-to-setup database connections, one per thread. Also,
you can set a backlog which can prevent an explosion of memory
usage if you have many jobs to start.

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

Using the state-making capability is optional. If you don't call the
`with_state` function, then your job closures don't need any parameters,
which therefor makes this crate compatible with other scoped threadpool
libraries.

# Example
    extern crate pond;
    let mut pool = pond::Pool::new();

    let mut vec = vec![0, 0, 0, 0, 0, 0, 0, 0];

    // Each thread can access the variables from
    // the current scope
    pool.scoped(
        |scoped|
        {
            let scoped = scoped.with_state(
                || "costly setup function".len()
            );
            // each thread runs the above setup function

            // Create references to each element in the vector ...
            for e in &mut vec
            {
                scoped.execute(
                    move |state|
                    {
                        *e += *state;
                        assert_eq!(*e, 21);
                    }
                );
            }
        }
    );

# Changelog

* 0.3.0 (2018-07-09): The constructor for `Pool` now in general defaults
to the native number of threads, and the backlog is no longer unbounded.
I have found that this makes things less error prone and less unnecessarily
verbose.

# See Also

* [scoped-threadpool](https://crates.io/crates/scoped-threadpool) (Has a very similar API, but no state). Please beware [a serious bug](https://github.com/Kimundi/scoped-threadpool-rs/issues/16).
* [scoped_pool](https://crates.io/crates/scoped_pool) (Very flexible, but no state)
* [crossbeam](https://crates.io/crates/crossbeam) (doesn't implement a thread pool)

