// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions for interacting with the host environment.
//!
//! The zkVM provides a set of functions to perform operations that manage
//! execution, I/O, and proof composition. The set of functions
//! related to each of these operations are described below.
//!
//! ## System State
//!
//! The guest has some control over the execution of the zkVM by pausing or
//! exiting the program explicitly. This can be achieved using the [pause] and
//! [exit] functions.
//!
//! ## Proof Verification
//!
//! The zkVM supports verification of RISC Zero [receipts] in a guest program,
//! enabling [proof composition]. This can be achieved using the [verify] and
//! [verify_integrity] functions.
//!
//! ## Input and Output
//!
//! The zkVM provides a set of functions for handling input, public output, and
//! private output. This is useful when interacting with the host and committing
//! to some data publicly.
//!
//! The zkVM provides functions that automatically perform (de)serialization on
//! types and, for performance reasons, there is also a `_slice` variant that
//! works with raw slices of plain old data. Performing operations on slices is
//! more efficient, saving cycles during execution and consequently producing
//! smaller proofs that are faster to produce. However, the `_slice` variants
//! can be less ergonomic, so consider trade-offs when choosing between the two.
//! For more information about guest optimization, see RISC Zero's [instruction
//! on guest optimization][guest-optimization]
//!
//! Convenience functions to read and write to default file descriptors are
//! provided. See [read], [write][write()], [commit] (and their `_slice`
//! variants) for more information.
//!
//! In order to access default file descriptors directly, see [stdin], [stdout],
//! [stderr] and [journal]. These file descriptors are either [FdReader] or
//! [FdWriter] instances, which can be used to read from or write to the host.
//! To read from or write into them, use the [Read] and [Write] traits.
//!
//! WARNING: Specifying a file descriptor with the same value of a default file
//! descriptor is not recommended and may lead to unexpected behavior. A list of
//! default file descriptors can be found in the [fileno] module.
//!
//! ## Utility
//!
//! The zkVM provides utility functions to log messages to the debug console and
//! to measure the number of processor cycles that have occurred since the guest
//! began. These can be achieved using the [log] and [cycle_count] functions.
//!
//! [receipts]: crate::Receipt
//! [proof composition]:https://www.risczero.com/blog/proof-composition
//! [guest-optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice

use core::{cell::OnceCell, fmt, mem::MaybeUninit};

use bytemuck::Pod;
use risc0_zkvm_platform::{
    align_up, fileno,
    syscall::{
        self, sys_alloc_words, sys_cycle_count, sys_halt, sys_input, sys_log, sys_pause, sys_read,
        sys_read_words, sys_verify, sys_verify_integrity, sys_write, syscall_2, SyscallName,
    },
    WORD_SIZE,
};
use serde::{de::DeserializeOwned, Serialize};

use crate::{
    serde::{Deserializer, Serializer, WordRead, WordWrite},
    sha::{
        rust_crypto::{Digest as _, Sha256},
        Digest, Digestible, DIGEST_WORDS,
    },
    Assumptions, ExitCode, InvalidExitCodeError, MaybePruned, Output, PrunedValueError,
    ReceiptClaim,
};

static mut HASHER: OnceCell<Sha256> = OnceCell::new();

/// Digest of the running list of [Assumptions], generated by the [verify] and
/// [verify_integrity] calls made by the guest.
static mut ASSUMPTIONS_DIGEST: MaybePruned<Assumptions> = MaybePruned::Pruned(Digest::ZERO);

/// A random 16 byte value initialized to random data, provided by the host, on
/// guest start and upon resuming from a pause. Setting this value ensures that
/// the total memory image has at least 128 bits of entropy, preventing
/// information leakage through the post-state digest.
static mut MEMORY_IMAGE_ENTROPY: [u32; 4] = [0u32; 4];

pub(crate) fn init() {
    unsafe {
        HASHER.set(Sha256::new()).unwrap();
        syscall::sys_rand(
            MEMORY_IMAGE_ENTROPY.as_mut_ptr(),
            MEMORY_IMAGE_ENTROPY.len(),
        )
    }
}

pub(crate) fn finalize(halt: bool, user_exit: u8) {
    unsafe {
        let hasher = HASHER.take();
        let journal_digest: Digest = hasher.unwrap().finalize().as_slice().try_into().unwrap();
        let output = Output {
            journal: MaybePruned::Pruned(journal_digest),
            assumptions: MaybePruned::Pruned(ASSUMPTIONS_DIGEST.digest()),
        };
        let output_words: [u32; 8] = output.digest().into();

        if halt {
            sys_halt(user_exit, &output_words)
        } else {
            sys_pause(user_exit, &output_words)
        }
    }
}

/// Terminate execution of the zkVM.
///
/// Use an exit code of 0 to indicate success, and non-zero to indicate an error.
pub fn exit(exit_code: u8) -> ! {
    finalize(true, exit_code);
    unreachable!();
}

/// Pause the execution of the zkVM.
///
/// Execution may be continued at a later time.
/// Use an exit code of 0 to indicate success, and non-zero to indicate an error.
pub fn pause(exit_code: u8) {
    finalize(false, exit_code);
    init();
}

/// Exchange data with the host.
pub fn syscall(syscall: SyscallName, to_host: &[u8], from_host: &mut [u32]) -> syscall::Return {
    unsafe {
        syscall_2(
            syscall,
            from_host.as_mut_ptr(),
            from_host.len(),
            to_host.as_ptr() as u32,
            to_host.len() as u32,
        )
    }
}

/// Verify there exists a receipt for an execution with `image_id` and `journal`.
///
/// Calling this function in the guest is logically equivalent to verifying a receipt with the same
/// image ID and journal. Any party verifying the receipt produced by this execution can then be
/// sure that the receipt verified by this call is also valid. In this way, multiple receipts from
/// potentially distinct guests can be combined into one. This feature is know as [composition].
///
/// In order to be valid, the [crate::Receipt] must have `ExitCode::Halted(0)` or
/// `ExitCode::Paused(0)`, an empty assumptions list, and an all-zeroes input hash. It may have any
/// post [crate::SystemState].
///
/// # Example
///
/// ```rust,ignore
/// use risc0_zkvm::guest::env;
///
/// # let HELLO_WORLD_ID = Digest::ZERO;
/// env::verify(HELLO_WORLD_ID, b"hello world".as_slice()).unwrap();
/// ```
///
/// [composition]: https://dev.risczero.com/terminology#composition
pub fn verify(image_id: impl Into<Digest>, journal: &[impl Pod]) -> Result<(), VerifyError> {
    let image_id: Digest = image_id.into();
    let journal_digest: Digest = bytemuck::cast_slice::<_, u8>(journal).digest();
    let mut from_host_buf = MaybeUninit::<[u32; DIGEST_WORDS + 1]>::uninit();

    unsafe {
        sys_verify(
            image_id.as_ref(),
            journal_digest.as_ref(),
            from_host_buf.as_mut_ptr(),
        )
    };

    // Split the host buffer into the Digest and system exit code portions. This is statically
    // known to succeed, but the array APIs that would allow compile-time checked splitting are
    // unstable.
    let (post_state_digest, sys_exit_code): (Digest, u32) = {
        let buf = unsafe { from_host_buf.assume_init() };
        let (digest_buf, code_buf) = buf.split_at(DIGEST_WORDS);
        (digest_buf.try_into().unwrap(), code_buf[0])
    };

    // Require that the exit code is either Halted(0) or Paused(0).
    let exit_code = ExitCode::from_pair(sys_exit_code, 0)?;
    if !exit_code.is_ok() {
        return Err(VerifyError::BadExitCodeResponse(InvalidExitCodeError(
            sys_exit_code,
            0,
        )));
    };

    // Construct the ReceiptClaim for this assumption. Use the host provided
    // post_state_digest and fix all fields that are required to have a certain
    // value. This assumption will only be resolvable if there exists a receipt
    // matching this claim.
    let assumption_claim = ReceiptClaim {
        pre: MaybePruned::Pruned(image_id),
        post: MaybePruned::Pruned(post_state_digest),
        exit_code,
        input: Digest::ZERO,
        output: Some(Output {
            journal: MaybePruned::Pruned(journal_digest),
            assumptions: MaybePruned::Pruned(Digest::ZERO),
        })
        .into(),
    };
    unsafe { ASSUMPTIONS_DIGEST.add(assumption_claim.into()) };

    Ok(())
}

/// Error encountered during a call to [verify].
///
/// Note that an error is only returned for "provable" errors. In particular, if
/// the host fails to find a receipt matching the requested image_id and
/// journal, this is not a provable error. In this case, the [verify] call
/// will not return.
#[derive(Debug)]
#[non_exhaustive]
pub enum VerifyError {
    /// Error returned when the host responds to `sys_verify` with an invalid exit code.
    BadExitCodeResponse(InvalidExitCodeError),
}

impl From<InvalidExitCodeError> for VerifyError {
    fn from(err: InvalidExitCodeError) -> Self {
        Self::BadExitCodeResponse(err)
    }
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::BadExitCodeResponse(err) => {
                write!(f, "bad response from host to sys_verify: {}", err)
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for VerifyError {}

/// Verify that there exists a valid receipt with the specified
/// [crate::ReceiptClaim].
///
/// Calling this function in the guest is logically equivalent to verifying a receipt with the same
/// [crate::ReceiptClaim]. Any party verifying the receipt produced by this execution can then be
/// sure that the receipt verified by this call is also valid. In this way, multiple receipts from
/// potentially distinct guests can be combined into one. This feature is know as [composition].
///
/// In order for a receipt to be valid, it must have a verifying cryptographic seal and
/// additionally have no assumptions. Note that executions with no output (e.g. those ending in
/// [ExitCode::SystemSplit]) will not have any encoded assumptions even if [verify] or
/// [verify_integrity] is called.
///
/// [composition]: https://dev.risczero.com/terminology#composition
pub fn verify_integrity(claim: &ReceiptClaim) -> Result<(), VerifyIntegrityError> {
    // Check that the assumptions list is empty.
    let assumptions_empty = claim.output.is_none()
        || claim
            .output
            .as_value()?
            .as_ref()
            .map_or(true, |output| output.assumptions.is_empty());

    if !assumptions_empty {
        return Err(VerifyIntegrityError::NonEmptyAssumptionsList);
    }

    let claim_digest = claim.digest();

    unsafe {
        sys_verify_integrity(claim_digest.as_ref());
        ASSUMPTIONS_DIGEST.add(MaybePruned::Pruned(claim_digest));
    }

    Ok(())
}

/// Error encountered during a call to [verify_integrity].
///
/// Note that an error is only returned for "provable" errors. In particular, if the host fails to
/// find a receipt matching the requested claim digest, this is not a provable error. In this
/// case, [verify_integrity] will not return.
#[derive(Debug)]
#[non_exhaustive]
pub enum VerifyIntegrityError {
    /// Provided [crate::ReceiptClaim] struct contained a non-empty assumptions list.
    ///
    /// This is a semantic error as only unconditional receipts can be verified
    /// inside the guest. If there is a conditional receipt to verify, it's
    /// assumptions must first be verified to make the receipt
    /// unconditional.
    NonEmptyAssumptionsList,

    /// Metadata output was pruned and not equal to the zero hash. It is
    /// impossible to determine whether the assumptions list is empty.
    PrunedValueError(PrunedValueError),
}

impl From<PrunedValueError> for VerifyIntegrityError {
    fn from(err: PrunedValueError) -> Self {
        Self::PrunedValueError(err)
    }
}

impl fmt::Display for VerifyIntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            VerifyIntegrityError::NonEmptyAssumptionsList => {
                write!(f, "assumptions list is not empty")
            }
            VerifyIntegrityError::PrunedValueError(err) => {
                write!(f, "claim output is pruned and non-zero: {}", err.0)
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for VerifyIntegrityError {}

/// Exchanges slices of plain old data with the host.
///
/// This makes two calls to the given syscall; the first gets the length of the
/// buffer to allocate for the return data, and the second actually
/// receives the return data.
///
/// On the host side, implement SliceIo to provide a handler for this call.
pub fn send_recv_slice<T: Pod, U: Pod>(syscall_name: SyscallName, to_host: &[T]) -> &'static [U] {
    let syscall::Return(nelem, _) = syscall(syscall_name, bytemuck::cast_slice(to_host), &mut []);
    let nwords = align_up(core::mem::size_of::<T>() * nelem as usize, WORD_SIZE) / WORD_SIZE;
    let from_host_buf = unsafe { core::slice::from_raw_parts_mut(sys_alloc_words(nwords), nwords) };
    syscall(syscall_name, &[], from_host_buf);
    &bytemuck::cast_slice(from_host_buf)[..nelem as usize]
}

/// Read private data from the STDIN of the zkVM and deserializes it.
///
/// This function operates on every [`DeserializeOwned`] type, so you can
/// specify complex types as data to be read and it'll be deserialized
/// automatically.
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
/// use std::collections::BTreeMap;
///
/// let input: Option<BTreeMap<u64, bool>> = env::read();
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
pub fn read<T: DeserializeOwned>() -> T {
    stdin().read()
}

/// Read a slice from the STDIN of the zkVM.
///
/// This function reads a slice of [plain old data][bytemuck::Pod], not
/// incurring in deserialization overhead. Recommended for performance
/// optimizations. For more context on this, see RISC Zero's [instructions on
/// guest optimization].
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
///
/// let len: usize = env::read();
/// let mut slice = vec![0u8; len];
/// env::read_slice(&mut slice);
///
/// assert_eq!(slice.len(), len);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
/// [instructions on guest optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice
pub fn read_slice<T: Pod>(slice: &mut [T]) {
    stdin().read_slice(slice)
}

/// Serialize the given data and write it to the STDOUT of the zkVM.
///
/// This is available to the host as the private output on the prover.
/// Some implementations, such as [risc0-r0vm] will also write the data to
/// the host's stdout file descriptor. It is not included in the receipt.
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
/// use std::collections::BTreeMap;
///
/// let output: BTreeMap<u64, bool> = BTreeMap::from([
///    (1, true),
///    (2, false),
/// ]);
///
/// env::write(&output);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
pub fn write<T: Serialize>(data: &T) {
    stdout().write(data)
}

/// Write the given slice to the STDOUT of the zkVM.
///
/// This is available to the host as the private output on the prover.
/// Some implementations, such as [risc0-r0vm] will also write the data to
/// the host's stdout file descriptor. It is not included in the receipt.
///
/// This function reads a slice of [plain old data][bytemuck::Pod], not
/// incurring in deserialization overhead. Recommended for performance
/// optimizations. For more context on this, see RISC Zero's [instructions on
/// guest optimization].
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
///
/// let slice = [1u8, 2, 3, 4];
/// env::write_slice(&slice);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
/// [instructions on guest optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice
pub fn write_slice<T: Pod>(slice: &[T]) {
    stdout().write_slice(slice);
}

/// Serialize the given data and commit it to the journal.
///
/// Data in the journal is included in the receipt and is available to the
/// verifier. It is considered "public" data.
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
/// use std::collections::BTreeMap;
///
/// let data: BTreeMap<u64, bool> = BTreeMap::from([
///   (1, true),
///   (2, false),
/// ]);
///
/// env::commit(&data);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
pub fn commit<T: Serialize>(data: &T) {
    journal().write(data)
}

/// Commit the given slice to the journal.
///
/// Data in the journal is included in the receipt and is available to the
/// verifier. It is considered "public" data.
///
/// This function reads a slice of [plain old data][bytemuck::Pod], not
/// incurring in deserialization overhead. Recommended for performance
/// optimizations. For more context on this, see RISC Zero's [instructions on
/// guest optimization].
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
///
/// let slice = [1u8, 2, 3, 4];
/// env::commit_slice(&slice);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
/// [instructions on guest optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice
pub fn commit_slice<T: Pod>(slice: &[T]) {
    journal().write_slice(slice);
}

/// Return the number of processor cycles that have occurred since the guest
/// began.
///
/// WARNING: The cycle count is provided by the host and is not checked by the zkVM circuit.
pub fn cycle_count() -> usize {
    sys_cycle_count()
}

/// Print a message to the debug console.
pub fn log(msg: &str) {
    let msg = msg.as_bytes();
    unsafe {
        sys_log(msg.as_ptr(), msg.len());
    }
}

/// Return a writer for STDOUT.
pub fn stdout() -> FdWriter<impl for<'a> Fn(&'a [u8])> {
    FdWriter::new(fileno::STDOUT, |_| {})
}

/// Return a writer for STDERR.
pub fn stderr() -> FdWriter<impl for<'a> Fn(&'a [u8])> {
    FdWriter::new(fileno::STDERR, |_| {})
}

/// Return a writer for the JOURNAL.
pub fn journal() -> FdWriter<impl for<'a> Fn(&'a [u8])> {
    FdWriter::new(fileno::JOURNAL, |bytes| {
        unsafe { HASHER.get_mut().unwrap_unchecked().update(bytes) };
    })
}

/// Return a reader for the standard input
pub fn stdin() -> FdReader {
    FdReader::new(fileno::STDIN)
}

/// Reads and deserializes objects
pub trait Read {
    /// Read data from the host.
    fn read<T: DeserializeOwned>(&mut self) -> T;

    /// Read raw data from the host.
    fn read_slice<T: Pod>(&mut self, buf: &mut [T]);
}

impl<R: Read + ?Sized> Read for &mut R {
    fn read<T: DeserializeOwned>(&mut self) -> T {
        (**self).read()
    }

    fn read_slice<T: Pod>(&mut self, buf: &mut [T]) {
        (**self).read_slice(buf)
    }
}

/// Provides a FdReader which can read from any file descriptor
pub struct FdReader {
    fd: u32,
}

impl FdReader {
    /// Creates a new FdReader reading from the given file descriptor.
    pub fn new(fd: u32) -> FdReader {
        FdReader { fd }
    }

    #[must_use = "read_bytes can potentially do a short read; this case should be handled."]
    fn read_bytes(&mut self, buf: &mut [u8]) -> usize {
        unsafe { sys_read(self.fd, buf.as_mut_ptr(), buf.len()) }
    }

    // Like read_bytes, but fills the buffer completely or until EOF occurs.
    #[must_use = "read_bytes_all can potentially return EOF; this case should be handled."]
    fn read_bytes_all(&mut self, mut buf: &mut [u8]) -> usize {
        let mut tot_read = 0;
        while !buf.is_empty() {
            let nread = self.read_bytes(buf);
            if nread == 0 {
                break;
            }
            tot_read += nread;
            (_, buf) = buf.split_at_mut(nread);
        }

        tot_read
    }
}

impl Read for FdReader {
    fn read<T: DeserializeOwned>(&mut self) -> T {
        T::deserialize(&mut Deserializer::new(self)).unwrap()
    }

    fn read_slice<T: Pod>(&mut self, buf: &mut [T]) {
        if let Ok(words) = bytemuck::try_cast_slice_mut(buf) {
            // Reading words performs significantly better if we're word aligned.
            self.read_words(words).unwrap();
        } else {
            let bytes = bytemuck::cast_slice_mut(buf);
            if self.read_bytes_all(bytes) != bytes.len() {
                panic!("{:?}", crate::serde::Error::DeserializeUnexpectedEnd);
            }
        }
    }
}

impl WordRead for FdReader {
    fn read_words(&mut self, words: &mut [u32]) -> crate::serde::Result<()> {
        let nread_bytes = unsafe { sys_read_words(self.fd, words.as_mut_ptr(), words.len()) };
        if nread_bytes == words.len() * WORD_SIZE {
            Ok(())
        } else {
            Err(crate::serde::Error::DeserializeUnexpectedEnd)
        }
    }

    fn read_padded_bytes(&mut self, bytes: &mut [u8]) -> crate::serde::Result<()> {
        if self.read_bytes_all(bytes) != bytes.len() {
            return Err(crate::serde::Error::DeserializeUnexpectedEnd);
        }

        let unaligned = bytes.len() % WORD_SIZE;
        if unaligned != 0 {
            let pad_bytes = WORD_SIZE - unaligned;
            let mut padding = [0u8; WORD_SIZE];
            if self.read_bytes_all(&mut padding[..pad_bytes]) != pad_bytes {
                return Err(crate::serde::Error::DeserializeUnexpectedEnd);
            }
        }
        Ok(())
    }
}

#[cfg(feature = "std")]
impl std::io::Read for FdReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(self.read_bytes(buf))
    }
}

/// Serializes and writes objects.
pub trait Write {
    /// Write a serialized object.
    fn write<T: Serialize>(&mut self, val: T);

    /// Write raw data.
    fn write_slice<T: Pod>(&mut self, buf: &[T]);
}

impl<W: Write + ?Sized> Write for &mut W {
    fn write<T: Serialize>(&mut self, val: T) {
        (**self).write(val)
    }

    fn write_slice<T: Pod>(&mut self, buf: &[T]) {
        (**self).write_slice(buf)
    }
}

/// Provides a FdWriter which can write to any file descriptor.
pub struct FdWriter<F: Fn(&[u8])> {
    fd: u32,
    hook: F,
}

impl<F: Fn(&[u8])> FdWriter<F> {
    /// Creates a new FdWriter writing to the given file descriptor.
    pub fn new(fd: u32, hook: F) -> Self {
        FdWriter { fd, hook }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        unsafe { sys_write(self.fd, bytes.as_ptr(), bytes.len()) }
        (self.hook)(bytes);
    }
}

impl<F: Fn(&[u8])> Write for FdWriter<F> {
    fn write<T: Serialize>(&mut self, val: T) {
        val.serialize(&mut Serializer::new(self)).unwrap();
    }

    fn write_slice<T: Pod>(&mut self, buf: &[T]) {
        self.write_bytes(bytemuck::cast_slice(buf));
    }
}

impl<F: Fn(&[u8])> WordWrite for FdWriter<F> {
    fn write_words(&mut self, words: &[u32]) -> crate::serde::Result<()> {
        self.write_bytes(bytemuck::cast_slice(words));
        Ok(())
    }

    fn write_padded_bytes(&mut self, bytes: &[u8]) -> crate::serde::Result<()> {
        self.write_bytes(bytes);
        let unaligned = bytes.len() % WORD_SIZE;
        if unaligned != 0 {
            let pad_bytes = WORD_SIZE - unaligned;
            self.write_bytes(&[0u8; WORD_SIZE][..pad_bytes]);
        }
        Ok(())
    }
}

#[cfg(feature = "std")]
impl<F: Fn(&[u8])> std::io::Write for FdWriter<F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Read the input digest from the input commitment.
pub fn input_digest() -> Digest {
    Digest::new([
        sys_input(0),
        sys_input(1),
        sys_input(2),
        sys_input(3),
        sys_input(4),
        sys_input(5),
        sys_input(6),
        sys_input(7),
    ])
}
