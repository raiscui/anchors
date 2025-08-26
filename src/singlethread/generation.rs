use std::num::NonZeroU64;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub struct Generation(NonZeroU64);
impl Generation {
    pub fn new() -> Generation {
        Generation(NonZeroU64::new(1).unwrap())
    }
    pub fn increment(&mut self) {
        // Rust 2024 将 `gen` 作为保留关键字使用，这里改为原始标识符避免冲突
        let r#gen: u64 = u64::from(self.0) + 1;
        self.0 = NonZeroU64::new(r#gen).unwrap();
    }
}
