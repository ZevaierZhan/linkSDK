//! Runs the process-wide SQLite syscall hook only in an exact-test subprocess.
//! Forces the real winShmLock check -> LockFileEx race, without replacing WAL.
use super::{store, DataStore, Operation};
use rusqlite::{ffi, Connection};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Barrier, OnceLock,
};

type LockFileEx = unsafe extern "system" fn(
    *mut std::ffi::c_void,
    u32,
    u32,
    u32,
    u32,
    *mut std::ffi::c_void,
) -> i32;
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static ARRIVALS: AtomicUsize = AtomicUsize::new(0);
static BARRIER: OnceLock<Barrier> = OnceLock::new();
unsafe extern "system" fn lock_hook(
    h: *mut std::ffi::c_void,
    flags: u32,
    reserved: u32,
    low: u32,
    high: u32,
    overlapped: *mut std::ffi::c_void,
) -> i32 {
    // OVERLAPPED: two pointer-sized words, then Offset/OffsetHigh.
    let offset = *((overlapped as *const u8).add(2 * std::mem::size_of::<usize>()) as *const u32);
    if ARMED.load(Ordering::SeqCst) && flags & 2 == 0 && offset == 122 && low == 1 {
        let slot = ARRIVALS.fetch_add(1, Ordering::SeqCst);
        if slot < 2 {
            BARRIER.get().unwrap().wait();
        }
    }
    let original: LockFileEx = std::mem::transmute(ORIGINAL.load(Ordering::SeqCst));
    original(h, flags, reserved, low, high, overlapped)
}
unsafe fn file(db: &Connection) -> *mut ffi::sqlite3_file {
    let mut p = std::ptr::null_mut();
    assert_eq!(
        ffi::sqlite3_file_control(
            db.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            &mut p as *mut _ as *mut _
        ),
        0
    );
    p
}
unsafe fn shm(db: &Connection, flags: i32) -> i32 {
    let f = file(db);
    ((*(*f).pMethods).xShmLock.unwrap())(f, 2, 1, flags)
}

#[test]
fn shared_handle_race_child() {
    if std::env::var_os("LINK_SDK_NATIVE_RACE_CHILD").is_none() {
        return;
    }
    let path = std::env::temp_dir().join(format!("link-sdk-native-race-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let directory = path.canonicalize().unwrap();
    let handle = DataStore::open_in(&directory, 123, &Operation::default()).unwrap();
    drop(handle);
    let (one, _) = store::Store::open_read_at(&directory, 123, true).unwrap();
    let (two, _) = store::Store::open_read_at(&directory, 123, true).unwrap();
    unsafe {
        let vfs = ffi::sqlite3_vfs_find(std::ptr::null());
        let original = ((*vfs).xGetSystemCall.unwrap())(vfs, c"LockFileEx".as_ptr()).unwrap();
        ORIGINAL.store(original as usize, Ordering::SeqCst);
        BARRIER.set(Barrier::new(2)).unwrap();
        assert_eq!(
            ((*vfs).xSetSystemCall.unwrap())(
                vfs,
                c"LockFileEx".as_ptr(),
                Some(std::mem::transmute(lock_hook as LockFileEx))
            ),
            0
        );
        ARMED.store(true, Ordering::SeqCst);
        let workers = [one, two].map(|s| {
            std::thread::spawn(move || {
                assert_eq!(shm(&s.db, ffi::SQLITE_SHM_LOCK | ffi::SQLITE_SHM_SHARED), 0);
                s
            })
        });
        let [a, b] = workers.map(|t| t.join().unwrap());
        ARMED.store(false, Ordering::SeqCst);
        assert_eq!(
            ((*vfs).xSetSystemCall.unwrap())(vfs, c"LockFileEx".as_ptr(), Some(original)),
            0
        );
        assert_eq!(ARRIVALS.load(Ordering::SeqCst), 2);
        assert_eq!(
            shm(&a.db, ffi::SQLITE_SHM_UNLOCK | ffi::SQLITE_SHM_SHARED),
            0
        );
        assert_eq!(
            shm(&b.db, ffi::SQLITE_SHM_UNLOCK | ffi::SQLITE_SHM_SHARED),
            0
        );
        let result = shm(&a.db, ffi::SQLITE_SHM_LOCK | ffi::SQLITE_SHM_EXCLUSIVE);
        if result == 0 {
            assert_eq!(
                shm(&a.db, ffi::SQLITE_SHM_UNLOCK | ffi::SQLITE_SHM_EXCLUSIVE),
                0
            );
        }
        // Fault injection: invalidate only the volatile WAL index header while
        // all SQL transactions are idle. SQLite must rebuild it from durable
        // DB/WAL. An orphaned recovery lock turns this into the original 15.
        let f = file(&a.db);
        let mut mapped = std::ptr::null_mut();
        assert_eq!(
            ((*(*f).pMethods).xShmMap.unwrap())(f, 0, 32768, 0, &mut mapped),
            0
        );
        assert!(!mapped.is_null());
        std::ptr::write_bytes(mapped as *mut u8, 0, 96);
        let began = std::time::Instant::now();
        let recovered = store::Store::open_read_at(&directory, 123, true);
        eprintln!(
            "controlled WAL recovery: lock_rc={result}, sqlite={:?}, elapsed_ms={}",
            recovered.as_ref().err().and_then(|e| e.sqlite_code),
            began.elapsed().as_millis()
        );
        assert!(
            recovered.is_ok(),
            "WAL recovery failed: {:?}",
            recovered.err()
        );
        assert_eq!(
            result, 0,
            "recovery byte remains OS-locked after both owners released it"
        );
        drop((a, b));
    }
    // A fresh process/connection must be able to recover and write after Drop.
    DataStore::open_in(&directory, 123, &Operation::default())
        .unwrap()
        .action(
            serde_json::json!({"op":"put","app":"fixture","dataset":"items","key":"k","value":1}),
            &Operation::default(),
        )
        .unwrap();
}

#[test]
fn recovery_lock_is_released_after_concurrent_shared_acquisition() {
    let original = unsafe {
        let vfs = ffi::sqlite3_vfs_find(std::ptr::null());
        ((*vfs).xGetSystemCall.unwrap())(vfs, c"LockFileEx".as_ptr()).unwrap() as usize
    };
    let mut child = ChildGuard::spawn(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "data::wal_windows_tests::shared_handle_race_child",
                "--nocapture",
            ])
            .env("LINK_SDK_NATIVE_RACE_CHILD", "1"),
    );
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("native race child exceeded outer collector limit");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "native SHM lock ownership regression in isolated child"
    );
    drop(child);
    unsafe {
        let vfs = ffi::sqlite3_vfs_find(std::ptr::null());
        assert_eq!(
            ((*vfs).xGetSystemCall.unwrap())(vfs, c"LockFileEx".as_ptr()).unwrap() as usize,
            original,
            "child syscall hook leaked into parent"
        );
    }
}

struct ChildGuard(std::process::Child);
impl ChildGuard {
    fn spawn(command: &mut std::process::Command) -> Self {
        Self(command.spawn().unwrap())
    }
}
impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
