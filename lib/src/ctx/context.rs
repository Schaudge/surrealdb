use crate::ctx::canceller::Canceller;
use crate::ctx::reason::Reason;
use crate::dbs::Transaction;
use crate::err::Error;
use crate::idx::planner::executor::QueryExecutor;
use crate::sql::value::Value;
use crate::sql::Thing;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use trice::Instant;

impl<'a> From<Value> for Cow<'a, Value> {
	fn from(v: Value) -> Cow<'a, Value> {
		Cow::Owned(v)
	}
}

impl<'a> From<&'a Value> for Cow<'a, Value> {
	fn from(v: &'a Value) -> Cow<'a, Value> {
		Cow::Borrowed(v)
	}
}
pub struct Context<'a> {
	// An optional parent context.
	parent: Option<&'a Context<'a>>,
	// An optional deadline.
	deadline: Option<Instant>,
	// Whether or not this context is cancelled.
	cancelled: Arc<AtomicBool>,
	// A collection of read only values stored in this context.
	values: HashMap<Cow<'static, str>, Cow<'a, Value>>,
	// An optional transaction
	transaction: Option<Transaction>,
	// An optional query executor
	query_executors: Option<Arc<HashMap<String, QueryExecutor>>>,
	// An optional record id
	thing: Option<&'a Thing>,
	// An optional cursor document
	cursor_doc: Option<&'a Value>,
}

impl<'a> Default for Context<'a> {
	fn default() -> Self {
		Context::background()
	}
}

impl<'a> Debug for Context<'a> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.debug_struct("Context")
			.field("parent", &self.parent)
			.field("deadline", &self.deadline)
			.field("cancelled", &self.cancelled)
			.field("values", &self.values)
			.field("thing", &self.thing)
			.field("doc", &self.cursor_doc)
			.finish()
	}
}

impl<'a> Context<'a> {
	/// Create an empty background context.
	pub fn background() -> Self {
		Context {
			values: HashMap::default(),
			parent: None,
			deadline: None,
			cancelled: Arc::new(AtomicBool::new(false)),
			transaction: None,
			query_executors: None,
			thing: None,
			cursor_doc: None,
		}
	}

	/// Create a new child from a frozen context.
	pub fn new(parent: &'a Context) -> Self {
		Context {
			values: HashMap::default(),
			parent: Some(parent),
			deadline: parent.deadline,
			cancelled: Arc::new(AtomicBool::new(false)),
			transaction: parent.transaction.clone(),
			query_executors: parent.query_executors.clone(),
			thing: parent.thing,
			cursor_doc: parent.cursor_doc,
		}
	}

	/// Add cancellation to the context. The value that is returned will cancel
	/// the context and it's children once called.
	pub fn add_cancel(&mut self) -> Canceller {
		let cancelled = self.cancelled.clone();
		Canceller::new(cancelled)
	}

	/// Add a deadline to the context. If the current deadline is sooner than
	/// the provided deadline, this method does nothing.
	pub fn add_deadline(&mut self, deadline: Instant) {
		match self.deadline {
			Some(current) if current < deadline => (),
			_ => self.deadline = Some(deadline),
		}
	}

	/// Add a timeout to the context. If the current timeout is sooner than
	/// the provided timeout, this method does nothing.
	pub fn add_timeout(&mut self, timeout: Duration) {
		self.add_deadline(Instant::now() + timeout)
	}

	pub fn add_transaction(&mut self, txn: Option<&Transaction>) {
		if let Some(txn) = txn {
			self.transaction = Some(txn.clone());
		}
	}

	pub fn add_thing(&mut self, thing: &'a Thing) {
		self.thing = Some(thing);
	}

	/// Add a cursor document to this context.
	/// Usage: A new child context is created by an iterator for each document.
	/// The iterator sets the value of the current document (known as cursor document).
	/// The cursor document is copied do the child contexts.
	pub(crate) fn add_cursor_doc(&mut self, doc: &'a Value) {
		self.cursor_doc = Some(doc);
	}

	/// Set the query executors
	pub(crate) fn set_query_executors(&mut self, executors: HashMap<String, QueryExecutor>) {
		self.query_executors = Some(Arc::new(executors));
	}

	/// Add a value to the context. It overwrites any previously set values
	/// with the same key.
	pub fn add_value<K, V>(&mut self, key: K, value: V)
	where
		K: Into<Cow<'static, str>>,
		V: Into<Cow<'a, Value>>,
	{
		self.values.insert(key.into(), value.into());
	}

	/// Get the timeout for this operation, if any. This is useful for
	/// checking if a long job should be started or not.
	pub fn timeout(&self) -> Option<Duration> {
		self.deadline.map(|v| v.saturating_duration_since(Instant::now()))
	}

	pub fn clone_transaction(&self) -> Result<Transaction, Error> {
		match &self.transaction {
			None => Err(Error::NoTx),
			Some(txn) => Ok(txn.clone()),
		}
	}

	pub fn thing(&self) -> Option<&Thing> {
		self.thing
	}

	pub fn doc(&self) -> Option<&Value> {
		self.cursor_doc
	}

	pub(crate) fn get_query_executor(&self, tb: &str) -> Option<&QueryExecutor> {
		if let Some(qe) = &self.query_executors {
			qe.get(tb)
		} else {
			None
		}
	}

	/// Check if the context is done. If it returns `None` the operation may
	/// proceed, otherwise the operation should be stopped.
	pub fn done(&self) -> Option<Reason> {
		match self.deadline {
			Some(deadline) if deadline <= Instant::now() => Some(Reason::Timedout),
			_ if self.cancelled.load(Ordering::Relaxed) => Some(Reason::Canceled),
			_ => match self.parent {
				Some(ctx) => ctx.done(),
				_ => None,
			},
		}
	}

	/// Check if the context is ok to continue.
	pub fn is_ok(&self) -> bool {
		self.done().is_none()
	}

	/// Check if the context is not ok to continue.
	pub fn is_done(&self) -> bool {
		self.done().is_some()
	}

	/// Check if the context is not ok to continue, because it timed out.
	pub fn is_timedout(&self) -> bool {
		matches!(self.done(), Some(Reason::Timedout))
	}

	/// Get a value from the context. If no value is stored under the
	/// provided key, then this will return None.
	pub fn value(&self, key: &str) -> Option<&Value> {
		match self.values.get(key) {
			Some(v) => match v {
				Cow::Borrowed(v) => Some(*v),
				Cow::Owned(v) => Some(v),
			},
			None => match self.parent {
				Some(p) => p.value(key),
				_ => None,
			},
		}
	}

	/// Get a 'static view into the cancellation status.
	#[cfg(feature = "scripting")]
	pub fn cancellation(&self) -> crate::ctx::cancellation::Cancellation {
		crate::ctx::cancellation::Cancellation::new(
			self.deadline,
			std::iter::successors(Some(self), |ctx| ctx.parent)
				.map(|ctx| ctx.cancelled.clone())
				.collect(),
		)
	}
}
