# Native focus lifetime model

The model covers two browser lifetimes and at most two focus losses per lifetime. Each loss may create one pending callback. Revocation and browser replacement may interleave with callback admission and invocation. A replacement is a different lifetime even when a label or native handle is reused.

The model does not establish that macOS or Windows generates every native focus transition. Native message delivery, Objective-C method dispatch, app activation, reparenting and HWND creation are abstracted as gain or loss signals. It also does not model callbacks that reenter the runtime.

The bug configuration separates the gate read and handler invocation. The serialized configuration models a UI owner checking the current browser registration and invoking the handler in one action, with revocation and replacement on that same owner. The matrix expects the former to violate `NoLateCallback` and the latter to satisfy it. The model and the proposed runtime correspondence have not been checked.
