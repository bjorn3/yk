#![feature(test)]
#![feature(register_attr)]
#![register_attr(trace_debug)]
#![register_attr(interp_step)]
#![register_attr(do_not_trace)]

#[cfg(test)]
mod helpers;

#[cfg(test)]
mod ykbh;

#[cfg(test)]
mod ykcompile;

#[cfg(test)]
mod yktrace;

#[cfg(test)]
mod guardfailure;
