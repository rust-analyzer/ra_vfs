# ra_vfs

[![Build Status](https://dev.azure.com/rust-analyzer/rust-analyzer/_apis/build/status/rust-analyzer.ra_vfs?branchName=master)](https://dev.azure.com/rust-analyzer/rust-analyzer/_build/latest?definitionId=1&branchName=master)

A virtual file system abstraction for rust-analyzer.

This lives outside of the main rust-analyzer repository because we want to
separate CI. VFS is hugely platform dependent, so CI for it tends to
be longer and more brittle.
