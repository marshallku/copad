// SwiftPM requires a non-header source so the clang module + linker
// settings flow through to the final executable. See sibling
// CCopadFFI/dummy.c for the rationale; same trick.
void _copad_term_dummy(void) {}
