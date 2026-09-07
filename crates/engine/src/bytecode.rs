pub use quickjs_oxide_core::bytecode::*;
#[cfg(any(test, feature = "test-support"))]
pub type BytecodeFunction = DetachedBytecode<crate::value::Value>;
#[cfg(any(test, feature = "test-support"))]
impl TestConstant for crate::value::Value {
    fn is_string(&self) -> bool {
        matches!(self, Self::String(_))
    }
}
