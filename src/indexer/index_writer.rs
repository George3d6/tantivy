use schema::Schema;
use schema::Document;
use indexer::SegmentSerializer;
use core::Index;
use core::SerializableSegment;
use core::Segment;
use std::thread::JoinHandle;
use indexer::SegmentWriter;
use std::clone::Clone;
use std::io;
use indexer::MergePolicy;
use std::thread;
use indexer::merger::IndexMerger;
use core::SegmentId;
use datastruct::stacker::Heap;
use std::mem::swap;
use chan;
use core::SegmentMeta;
use super::super::core::index::get_segment_manager;
use super::segment_manager::{SegmentManager, get_segment_ready_for_commit};
use Result;
use Error;

// Size of the margin for the heap. A segment is closed when the remaining memory
// in the heap goes below MARGIN_IN_BYTES.
pub const MARGIN_IN_BYTES: u32 = 10_000_000u32;

// We impose the memory per thread to be at least 30 MB.
pub const HEAP_SIZE_LIMIT: u32 = MARGIN_IN_BYTES * 3u32;

// Add document will block if the number of docs waiting in the queue to be indexed reaches PIPELINE_MAX_SIZE_IN_DOCS
const PIPELINE_MAX_SIZE_IN_DOCS: usize = 10_000;


type DocumentSender = chan::Sender<Document>;
type DocumentReceiver = chan::Receiver<Document>;

type SegmentUpdateSender = chan::Sender<SegmentUpdate>;
type SegmentUpdateReceiver = chan::Receiver<SegmentUpdate>;

/// `IndexWriter` is the user entry-point to add document to an index.
///
/// It manages a small number of indexing thread, as well as a shared
/// indexing queue.
/// Each indexing thread builds its own independant `Segment`, via
/// a `SegmentWriter` object.
pub struct IndexWriter {
	index: Index,
	heap_size_in_bytes_per_thread: usize,
	
	workers_join_handle: Vec<JoinHandle<Result<()>>>,
	
	document_receiver: DocumentReceiver,
	document_sender: DocumentSender,

	segment_update_sender: SegmentUpdateSender,

	num_threads: usize,
	docstamp: u64,

	merge_policy: Box<MergePolicy>,
}


fn index_documents(heap: &mut Heap,
				   segment: Segment,
				   schema: &Schema,
				   document_iterator: &mut Iterator<Item=Document>,
				   segment_update_sender: &mut SegmentUpdateSender) -> Result<()> {
	heap.clear();
	let segment_id = segment.id();
	let mut segment_writer = try!(SegmentWriter::for_segment(heap, segment, &schema));
	for doc in document_iterator {
		try!(segment_writer.add_document(&doc, &schema));
		if segment_writer.is_buffer_full() {
			info!("Buffer limit reached, flushing segment with maxdoc={}.", segment_writer.max_doc());
			break;
		}
	}
	let num_docs = segment_writer.max_doc() as usize;
	let segment_meta = SegmentMeta {
		segment_id: segment_id,
		num_docs: num_docs,
	};

	try!(segment_writer.finalize());
	segment_update_sender.send(SegmentUpdate::AddSegment(segment_meta));
	Ok(())
}


#[derive(Debug)]
pub enum SegmentUpdate {
    AddSegment(SegmentMeta),
    StartMerge(Vec<SegmentId>),
    EndMerge(Vec<SegmentId>, SegmentMeta),
    CancelGeneration,
    NewGeneration,
}

// Process a single segment update.
//
// If the segment manager has been changed a result,
// return true. (else return false)
fn process_segment_update(
		index: &Index,
		segment_manager: &SegmentManager,
		segment_update: SegmentUpdate,
		is_cancelled_generation: &mut bool) -> Result<bool> {
	match segment_update {
		SegmentUpdate::AddSegment(segment_meta) => {
			if !*is_cancelled_generation {
				segment_manager.add_segment(segment_meta);
			}
			else {
				index.delete_segment(segment_meta.segment_id);
			}
			Ok(true)
		},
		SegmentUpdate::StartMerge(segment_ids) => {
			if !*is_cancelled_generation {
				segment_manager.start_merge(&segment_ids);
				// TODO spawn a segment merge thread
			}
			Ok(false)
		},
		SegmentUpdate::EndMerge(segment_ids, segment_meta) => {
			segment_manager.end_merge(&segment_ids, &segment_meta);
			for segment_id in segment_ids {
				index.delete_segment(segment_id);
			}
			Ok(true)
		},
		SegmentUpdate::CancelGeneration => {
			*is_cancelled_generation = true;
			Ok(false)
		},
		SegmentUpdate::NewGeneration => {
			*is_cancelled_generation = false;
			Ok(false)
		}
	}
}
 
fn consider_merge_options(index: &mut Index, merge_policy: &MergePolicy) {
	let segment_manager = get_segment_manager(index);
	let (committed_segments, uncommitted_segments) = get_segment_ready_for_commit(&*segment_manager);
	// committed segments cannot be merged with uncommitted_segments.
	let merge_candidates_committed = merge_policy.compute_merge_candidates(&committed_segments);
	let merge_candidates_uncommitted = merge_policy.compute_merge_candidates(&uncommitted_segments);
	merge_candidates_committed.into_iter().chain(merge_candidates_uncommitted)
	.map(|merge_candidate| {
		println!("{:?}", merge_candidate);
	});
}

fn on_segment_change(index: &mut Index,
				     merge_policy: &MergePolicy) -> Result<()> {
	// saving the meta file.
	try!(index.save_metas());
	// update the searcher so that they eventually will
	// use the new segments.
	try!(index.load_searchers());
	// consider merge options.
	consider_merge_options(index, merge_policy);
	Ok(())
}

// Consumes the `segment_update_receiver` channel
// for segment updates and apply them.
//
// Using a channel ensures that all of the updates
// happen in the same thread, and makes
// the implementation of rollback and commit 
// trivial.
fn process_segment_updates(mut index: Index,
						   segment_manager: &SegmentManager,
						   segment_update_receiver: SegmentUpdateReceiver) -> Result<()> {
	let mut segment_update_it = segment_update_receiver.into_iter();
	let mut is_cancelled_generation = false;
	let merge_policy = index.get_merge_policy();
	loop {
		if let Some(segment_update) = segment_update_it.next() {
			let has_changed = try!(
				process_segment_update(
					&index,
					segment_manager,
					segment_update,
					&mut is_cancelled_generation)
			);
			if has_changed {
				on_segment_change(&mut index, &*merge_policy);
			}
		}
		else {
			// somehow, the channel was dropped.
			return Ok(());
		}
	}
}

impl IndexWriter {

	/// Spawns a new worker thread for indexing.
	/// The thread consumes documents from the pipeline.
	///
	fn add_indexing_worker(&mut self,) -> Result<()> {
		let index = self.index.clone();
		let schema = self.index.schema();
		
		let document_receiver_clone = self.document_receiver.clone();
		let mut segment_update_sender = self.segment_update_sender.clone();

		let mut heap = Heap::with_capacity(self.heap_size_in_bytes_per_thread); 
		let join_handle: JoinHandle<Result<()>> = thread::spawn(move || {
			loop {
				let segment = index.new_segment();
				let mut document_iterator = document_receiver_clone
					.clone()
					.into_iter()
					.peekable();
				// the peeking here is to avoid
				// creating a new segment's files 
				// if no document are available.
				if document_iterator.peek().is_some() {
					try!(
						index_documents(
							&mut heap,
							segment,
							&schema,
							&mut document_iterator,
							&mut segment_update_sender)
					);
				}
				else {
					// No more documents.
					// Happens when there is a commit, or if the `IndexWriter`
					// was dropped.
					return Ok(());
				}
			}
		});
		self.workers_join_handle.push(join_handle);

		Ok(())
	}

	fn on_change(&mut self,) -> Result<()> {
		try!(self.index.save_metas());
    	try!(self.index.load_searchers());
		Ok(())
	}
	
	/// Open a new index writer
	/// 
	/// num_threads tells the number of indexing worker that 
	/// should work at the same time.
	pub fn open(index: &Index,
				num_threads: usize,
				heap_size_in_bytes_per_thread: usize) -> Result<IndexWriter> {
		if heap_size_in_bytes_per_thread <= HEAP_SIZE_LIMIT as usize {
			panic!(format!("The heap size per thread needs to be at least {}.", HEAP_SIZE_LIMIT));
		}
		let (document_sender, document_receiver): (DocumentSender, DocumentReceiver) = chan::sync(PIPELINE_MAX_SIZE_IN_DOCS);
		let (segment_update_sender, segment_update_receiver): (SegmentUpdateSender, SegmentUpdateReceiver) = chan::sync(0);
		
		let segment_manager = get_segment_manager(index);

		let index_clone = index.clone();
		thread::spawn(move || {
			process_segment_updates(index_clone, &*segment_manager, segment_update_receiver)
		});

		let mut index_writer = IndexWriter {
			heap_size_in_bytes_per_thread: heap_size_in_bytes_per_thread,
			index: index.clone(),
			
			document_receiver: document_receiver,
			document_sender: document_sender,

			segment_update_sender: segment_update_sender,
			
			workers_join_handle: Vec::new(),
			num_threads: num_threads,

			merge_policy: index.get_merge_policy(),
			docstamp: index.docstamp(),
		};
		try!(index_writer.start_workers());
		Ok(index_writer)
	}

	fn start_workers(&mut self,) -> Result<()> {
		for _ in 0 .. self.num_threads {
			try!(self.add_indexing_worker());
		}
		Ok(())
	}
	
	/// Merges a given list of segments
	pub fn merge(&mut self, segments: &[Segment]) -> Result<()> {
		//  TODO fix commit or uncommited?
		let schema = self.index.schema();
		// An IndexMerger is like a "view" of our merged segments. 
		let merger = try!(IndexMerger::open(schema, segments));
		let mut merged_segment = self.index.new_segment();
		// ... we just serialize this index merger in our new segment
		// to merge the two segments.
		let segment_serializer = try!(SegmentSerializer::for_segment(&mut merged_segment));
		let num_docs = try!(merger.write(segment_serializer));
		let merged_segment_ids: Vec<SegmentId> = segments.iter().map(|segment| segment.id()).collect();
		let segment_meta = SegmentMeta {
			segment_id: merged_segment.id(),
			num_docs: num_docs,
		};
		let segment_manager = get_segment_manager(&self.index);
		segment_manager.end_merge(&merged_segment_ids, &segment_meta);
		try!(self.index.load_searchers());
		Ok(())
	}

	/// Closes the current document channel send.
	/// and replace all the channels by new ones.
	///
	/// The current workers will keep on indexing
	/// the pending document and stop 
	/// when no documents are remaining.
	///
	/// Returns the former segment_ready channel.  
	fn recreate_channel(&mut self,) -> DocumentReceiver {
		let (mut document_sender, mut document_receiver): (DocumentSender, DocumentReceiver) = chan::sync(PIPELINE_MAX_SIZE_IN_DOCS);
		swap(&mut self.document_sender, &mut document_sender);
		swap(&mut self.document_receiver, &mut document_receiver);
		document_receiver
	}

	/// Rollback to the last commit
	///
	/// This cancels all of the update that
	/// happened before after the last commit.
	/// After calling rollback, the index is in the same 
	/// state as it was after the last commit.
	///
	/// The docstamp at the last commit is returned. 
	pub fn rollback(&mut self,) -> Result<u64> {

		self.segment_update_sender.send(SegmentUpdate::CancelGeneration);
		
		// we cannot drop segment ready receiver yet
		// as it would block the workers.
		let document_receiver = self.recreate_channel();
		
		// Drains the document receiver pipeline :
		// Workers don't need to index the pending documents.
		for _ in document_receiver {};
		
		let mut former_workers_join_handle = Vec::new();
		swap(&mut former_workers_join_handle, &mut self.workers_join_handle);
		
		// wait for all the worker to finish their work
		// (it should be fast since we consumed all pending documents)
		for worker_handle in former_workers_join_handle {
			// we stop one worker at a time ...
			try!(try!(
				worker_handle
					.join()
					.map_err(|e| Error::ErrorInThread(format!("{:?}", e)))
			));
			// ... and recreate a new one right away
			// to work on the next generation.
			try!(self.add_indexing_worker());
		}

		// All of our indexing workers for the rollbacked generation have
		// been terminated.
		// Our document receiver pipe was drained.
		// No new document have been added in the meanwhile because `IndexWriter`
		// is not shared by different threads.
		//
		// We can now open a new generation and reaccept segments
		// from now on.
		self.segment_update_sender.send(SegmentUpdate::NewGeneration);

		let rollbacked_segments = get_segment_manager(&self.index).rollback();
		for segment_id in rollbacked_segments {
			self.index.delete_segment(segment_id);
		}
		try!(self.on_change());

		// reset the docstamp to what it was before
		self.docstamp = self.index.docstamp();
		Ok(self.docstamp)
	}


	/// Commits all of the pending changes
	/// 
	/// A call to commit blocks. 
	/// After it returns, all of the document that
	/// were added since the last commit are published 
	/// and persisted.
	///
	/// In case of a crash or an hardware failure (as 
	/// long as the hard disk is spared), it will be possible
	/// to resume indexing from this point.
	///
	/// Commit returns the `docstamp` of the last document
	/// that made it in the commit.
	///
	pub fn commit(&mut self,) -> Result<u64> {
		
		// this will drop the current channel
		self.recreate_channel();
		
		// Docstamp of the last document in this commit.
		let commit_docstamp = self.docstamp;

		let mut former_workers_join_handle = Vec::new();
		swap(&mut former_workers_join_handle, &mut self.workers_join_handle);
		
		for worker_handle in former_workers_join_handle {
			let indexing_worker_result = try!(worker_handle
				.join()
				.map_err(|e| Error::ErrorInThread(format!("{:?}", e)))
			);
			try!(indexing_worker_result);
			// add a new worker for the next generation.
			try!(self.add_indexing_worker());
		}

		super::super::core::index::commit(&mut self.index, commit_docstamp);
		try!(self.on_change());
		Ok(commit_docstamp)
	}
	

	/// Adds a document.
	///
	/// If the indexing pipeline is full, this call may block.
	/// 
	/// The docstamp is an increasing `u64` that can
	/// be used by the client to align commits with its own
	/// document queue.
	/// 
	/// Currently it represents the number of documents that 
	/// have been added since the creation of the index. 
	pub fn add_document(&mut self, doc: Document) -> io::Result<u64> {
		self.document_sender.send(doc);
		self.docstamp += 1;
		Ok(self.docstamp)
	}
	

}

#[cfg(test)]
mod tests {

	use schema::{self, Document};
	use Index;
	use Term;

	#[test]
	fn test_commit_and_rollback() {
		let mut schema_builder = schema::SchemaBuilder::default();
		let text_field = schema_builder.add_text_field("text", schema::TEXT);
		let index = Index::create_in_ram(schema_builder.build());
		let num_docs_containing = |s: &str| {
			let searcher = index.searcher();
			let term_a = Term::from_field_text(text_field, s);
			searcher.doc_freq(&term_a)
		};
		
		{
			// writing the segment
			let mut index_writer = index.writer_with_num_threads(1, 40_000_000).unwrap();
			{
				let mut doc = Document::default();
				doc.add_text(text_field, "a");
				index_writer.add_document(doc).unwrap();
				index_writer.commit().expect("commit failed");
			}
			{
				let mut doc = Document::default();
				doc.add_text(text_field, "a");
				index_writer.add_document(doc).unwrap();
				// here we have a partial segment.
			}
			{
				let mut doc = Document::default();
				doc.add_text(text_field, "a");
				index_writer.add_document(doc).unwrap();
				// here we have a partial segment.
			}
			assert_eq!(index_writer.rollback().unwrap(), 1u64);
			assert_eq!(num_docs_containing("a"), 1);
			
			{
				let mut doc = Document::default();
				doc.add_text(text_field, "b");
				index_writer.add_document(doc).unwrap();
			}
			{
				let mut doc = Document::default();
				doc.add_text(text_field, "c");
				index_writer.add_document(doc).unwrap();
			}
			assert_eq!(index_writer.commit().unwrap(), 3u64);
			assert_eq!(num_docs_containing("a"), 1);
			assert_eq!(num_docs_containing("b"), 1);
			assert_eq!(num_docs_containing("c"), 1);
		}
		index.searcher();
	}

}