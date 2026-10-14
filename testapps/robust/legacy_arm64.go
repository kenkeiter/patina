package main

// arm64 has no pipe/dup2/poll/eventfd/epoll_create/epoll_wait/pause.
func checkLegacyEntryPoints() {}
func checkPause()             {}
