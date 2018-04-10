// Copyright 2018 Manuel Holtgrewe, Berlin Institute of Health.
// Licensed under the MIT license (http://opensource.org/licenses/MIT)
// This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi;
use std::mem;
use std::ptr;
use std::path::Path;
use url::Url;
use libc;

use htslib;

/// A trait for a Tabix reader with a read method.
pub trait Read: Sized {
    /// Read next line into the given `Vec<u8>` (i.e., ASCII string).
    /// Use this method in combination with a single allocated record to avoid the reallocations
    /// occurring with the iterator.
    ///
    /// # Arguments
    ///
    /// * `record` - the `Vec<u8>` to be filled
    fn read(&mut self, record: &mut Vec<u8>) -> Result<(), ReadError>;

    /// Iterator over the lines/records of the seeked region.
    /// Note that, while being convenient, this is less efficient than pre-allocating a
    /// `Vec<u8>` and reading into it with the `read` method, since every iteration involves
    /// the allocation of a new `Vec<u8>`.
    fn records(&mut self) -> Records<Self>;

    /// Return the text headers, split by line.
    fn header(&self) -> &Vec<String>;
}

/// A Tabix file reader.
///
/// This struct and its associated functions are meant for reading plain-text tabix files.  That
/// is, any bgzip-ed and tabix-indexed file can be queried for regions.
////
/// Note that the `tabix` command from `htslib` can actually several more things, including
/// building indices and converting BCF to VCF text output.  Both is out of scope here.
#[derive(Debug)]
pub struct Reader {
    /// The header lines (if any).
    header: Vec<String>,

    /// The file to read from.
    hts_file: *mut htslib::htsFile,
    /// The file format information.
    hts_format: htslib::htsExactFormat,
    /// The tbx_t structure to read from.
    tbx: *mut htslib::tbx_t,
    /// The current buffer.
    buf: htslib::kstring_t,
    /// Iterator over the buffer.
    itr: Option<*mut htslib::hts_itr_t>,

    /// The currently fetch region's tid.
    tid: i32,
    /// The currently fetch region's 0-based begin pos.
    start: i32,
    /// The currently fetch region's 0-based end pos.
    end: i32,
}

unsafe impl Send for Reader {}

/// Redefinition of `KS_SEP_LINE` from `htslib/kseq.h`.
const KS_SEP_LINE: i32 = 2;

impl Reader {
    /// Create a new Reader from path.
    ///
    /// # Arguments
    ///
    /// * `path` - the path to open.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, TabixReaderPathError> {
        if let Some(p) = path.as_ref().to_str() {
            Ok(try!(Self::new(p.as_bytes())))
        } else {
            Err(TabixReaderPathError::InvalidPath)
        }
    }

    pub fn from_url(url: &Url) -> Result<Self, TabixReaderError> {
        Self::new(url.as_str().as_bytes())
    }

    /// Create a new Reader.
    ///
    /// # Arguments
    ///
    /// * `path` - the path.
    fn new(path: &[u8]) -> Result<Self, TabixReaderError> {
        let path = ffi::CString::new(path).unwrap();
        let hts_file =
            unsafe { htslib::hts_open(path.as_ptr(), ffi::CString::new("r").unwrap().as_ptr()) };
        let hts_format = unsafe { (*htslib::hts_get_format(hts_file)).format };
        let tbx = unsafe { htslib::tbx_index_load(path.as_ptr()) };
        let mut header = Vec::new();
        let mut buf = htslib::kstring_t {
            l: 0,
            m: 0,
            s: ptr::null_mut(),
        };
        unsafe {
            while htslib::hts_getline(hts_file, KS_SEP_LINE, &mut buf) >= 0 {
                if buf.l > 0 && (*buf.s) as i32 == (*tbx).conf.meta_char {
                    header.push(String::from(ffi::CStr::from_ptr(buf.s).to_str().unwrap()));
                } else {
                    break;
                }
            }
        }

        if tbx.is_null() {
            Err(TabixReaderError::InvalidIndex)
        } else {
            Ok(Reader {
                header,
                hts_file,
                hts_format,
                tbx,
                buf,
                itr: None,
                tid: -1,
                start: -1,
                end: -1,
            })
        }
    }

    /// Get sequence ID from sequence name.
    pub fn seq_name_to_id(&self, name: &str) -> Result<u32, SequenceLookupError> {
        // TODO: naming?
        let res = unsafe {
            htslib::tbx_name2id(
                self.tbx,
                ffi::CString::new(name.as_bytes()).unwrap().as_ptr(),
            )
        };
        if res < 0 {
            Err(SequenceLookupError::Some)
        } else {
            Ok(res as u32)
        }
    }

    /// Fetch region given by numeric sequence number and 0-based begin and end position.
    pub fn fetch(&mut self, tid: u32, start: u32, end: u32) -> Result<(), FetchError> {
        self.tid = tid as i32;
        self.start = start as i32;
        self.end = end as i32;

        if let Some(itr) = self.itr {
            unsafe {
                htslib::hts_itr_destroy(itr);
            }
        }
        let itr = unsafe {
            htslib::hts_itr_query(
                (*self.tbx).idx,
                tid as i32,
                start as i32,
                end as i32,
                Some(htslib::tbx_readrec),
            )
        };
        if itr.is_null() {
            self.itr = None;
            Err(FetchError::Some)
        } else {
            self.itr = Some(itr);
            Ok(())
        }
    }

    /// Return the sequence contig names.
    pub fn seqnames(&self) -> Vec<String> {
        let mut result = Vec::new();

        let mut nseq: i32 = 0;
        let seqs = unsafe { htslib::tbx_seqnames(self.tbx, &mut nseq) };
        for i in 0..nseq {
            unsafe {
                result.push(String::from(
                    ffi::CStr::from_ptr(*seqs.offset(i as isize))
                        .to_str()
                        .unwrap(),
                ));
            }
        }
        unsafe {
            libc::free(seqs as (*mut libc::c_void));
        };

        result
    }
}

/// Return whether the two given genomic intervals overlap.
fn overlap(tid1: i32, begin1: i32, end1: i32, tid2: i32, begin2: i32, end2: i32) -> bool {
    (tid1 == tid2) && (begin1 < end2) && (begin2 < end1)
}

impl Read for Reader {
    fn read(&mut self, record: &mut Vec<u8>) -> Result<(), ReadError> {
        match self.itr {
            Some(itr) => {
                loop {
                    // Try to read next line.
                    let ret = unsafe {
                        htslib::hts_itr_next(
                            htslib::hts_get_bgzfp(self.hts_file),
                            itr,
                            mem::transmute(&mut self.buf),
                            mem::transmute(self.tbx),
                        )
                    };
                    // Handle errors first.
                    if ret == -1 {
                        return Err(ReadError::NoMoreRecord);
                    } else if ret == -2 {
                        return Err(ReadError::Truncated);
                    } else if ret < 0 {
                        panic!("Return value should not be <0 but was: {}", ret);
                    }
                    // Return first overlapping record (loop will stop when `hts_itr_next(...)`
                    // returns `< 0`).
                    let (tid, start, end) =
                        unsafe { ((*itr).curr_tid, (*itr).curr_beg, (*itr).curr_end) };
                    if overlap(self.tid, self.start, self.end, tid, start, end) {
                        *record =
                            unsafe { Vec::from(ffi::CStr::from_ptr(self.buf.s).to_str().unwrap()) };
                        return Ok(());
                    }
                }
            }
            _ => Err(ReadError::NoIter),
        }
    }

    fn records(&mut self) -> Records<Self> {
        Records { reader: self }
    }

    fn header(&self) -> &Vec<String> {
        &self.header
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            if self.itr.is_some() {
                htslib::hts_itr_destroy(self.itr.unwrap());
            }
            htslib::tbx_destroy(self.tbx);
            htslib::hts_close(self.hts_file);
        }
    }
}

/// Iterator over the lines of a tabix file.
#[derive(Debug)]
pub struct Records<'a, R: 'a + Read> {
    reader: &'a mut R,
}

impl<'a, R: Read> Iterator for Records<'a, R> {
    type Item = Result<Vec<u8>, ReadError>;

    fn next(&mut self) -> Option<Result<Vec<u8>, ReadError>> {
        let mut record = Vec::new();
        match self.reader.read(&mut record) {
            Err(ReadError::NoMoreRecord) => None,
            Ok(()) => Some(Ok(record)),
            Err(err) => Some(Err(err)),
        }
    }
}

quick_error! {
    #[derive(Debug, Clone)]
    pub enum ReadError {
        NoIter {
            description("previous iterator generation failed")
        }
        Truncated {
            description("truncated record")
        }
        Invalid {
            description("invalid record")
        }
        NoMoreRecord {
            description("no more record")
        }
    }
}

impl ReadError {
    /// Returns true if no record has been read because the end of the file was reached.
    pub fn is_eof(&self) -> bool {
        match self {
            &ReadError::NoMoreRecord => true,
            _ => false,
        }
    }
}

quick_error! {
    #[derive(Debug, Clone)]
    pub enum TabixReaderError {
        InvalidIndex {
            description("invalid index")
        }
        BGZFError(err: BGZFError) {
            from()
        }
    }
}

quick_error! {
    #[derive(Debug, Clone)]
    pub enum TabixReaderPathError {
        InvalidPath {
            description("invalid path")
        }
        TabixReaderError(err: TabixReaderError) {
            from()
        }
    }
}

quick_error! {
    #[derive(Debug, Clone)]
    pub enum BGZFError {
        Some {
            description("error reading BGZF file")
        }
    }
}

quick_error! {
    #[derive(Debug, Clone)]
    pub enum FetchError {
        Some {
            description("error fetching a locus")
        }
    }
}

quick_error! {
    #[derive(Debug, Clone)]
    pub enum SequenceLookupError {
        Some {
            description("error looking up a sequence name")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bed_header() {
        let reader = Reader::from_path("test/test_bed3.bed.gz").ok().expect(
            "Error opening file.",
        );

        // Check header lines.
        assert_eq!(
            reader.header,
            vec![String::from("#foo"), String::from("#bar")]
        );

        // Check sequence name vector.
        assert_eq!(
            reader.seqnames(),
            vec![String::from("chr1"), String::from("chr2")]
        );

        // Check mapping between name and idx.
        assert_eq!(reader.seq_name_to_id("chr1").unwrap(), 0);
        assert_eq!(reader.seq_name_to_id("chr2").unwrap(), 1);
        assert!(reader.seq_name_to_id("chr3").is_err());
    }

    #[test]
    fn bed_fetch_from_chr1_read_api() {
        let mut reader = Reader::from_path("test/test_bed3.bed.gz").ok().expect(
            "Error opening file.",
        );

        let chr1_id = reader.seq_name_to_id("chr1").unwrap();
        assert!(reader.fetch(chr1_id, 1000, 1003).is_ok());

        let mut record = Vec::new();
        assert!(reader.read(&mut record).is_ok());
        assert_eq!(record, Vec::from("chr1\t1001\t1002"));
        assert!(reader.read(&mut record).is_err());
    }

    #[test]
    fn bed_fetch_from_chr1_iterator_api() {
        let mut reader = Reader::from_path("test/test_bed3.bed.gz").ok().expect(
            "Error opening file.",
        );

        let chr1_id = reader.seq_name_to_id("chr1").unwrap();
        assert!(reader.fetch(chr1_id, 1000, 1003).is_ok());

        let records: Vec<Vec<u8>> = reader.records().map(|r| r.unwrap()).collect();
        assert_eq!(records, vec![Vec::from("chr1\t1001\t1002")]);
    }
}
