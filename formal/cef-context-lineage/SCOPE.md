# Scope

A renderer's delayed context registration must never grant authority after the native document has been replaced or released. The bounded model includes two document lifetimes, same-process and process-swap assignments, renderer termination, asynchronous registration, replacement, message loss, delayed delivery, exact release and a separate activation acknowledgement that can arrive after revocation. It abstracts frames and capability bytes into document identities. It does not establish that the proposed native identity guard is implemented. Native tests and TLC are not run locally.
