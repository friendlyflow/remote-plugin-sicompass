# Changelog

## 0.3.2

- A server's rows are shown exactly as written except its links and pictures, and a key holding a slash opens the right page.
- Needs sicompass 0.2.4.
- Intel macs are not built any more.

## 0.3.1

- Scroll mode no longer reads ahead into levels you have not opened. Each level
  is a request to your server, so it waits until you open it.

## 0.3.0

Remote is a program of its own now, instead of a sandboxed WebAssembly component.
Sicompass starts it and talks to it, one per tab, and it runs with your rights, so
its entry in the Store says what it does before you install it, and installing it is
your approval.

- It reads lists from servers you name, and connects to nothing else. Requests now time out after 30 seconds.
- One build for each of Linux (x86_64 and arm64, static), macOS (Apple Silicon and
  Intel) and Windows.
- Needs a Sicompass that runs plugin programs. An older Sicompass keeps the 0.2
  version it has.
