#!/usr/bin/env python3
"""One wire request, made from INSIDE a pane.

The frame harness's own connection is never a pane, so every message it sends
itself is refused `sender_unverified`. Any case that needs a real sender runs
this script inside a pane, where the server attributes it to that pane.

The request is read from a FILE, never from an argument or typed bytes: a
message body then appears on no command line and in no pane's input.

usage: wire_send.py <socket> <in-file> <out-file>

Writes the first reply that is not a `ViewList` to <out-file> as one JSON
line, by rename so a poller never reads half of it, and exits 0.

A JSON LIST in <in-file> is sent request by request on the one connection,
each answered before the next goes, and the replies are written as a list.
That is how a case puts several sends inside one rate window without
depending on how fast a shell starts python.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, name_of  # noqa: E402


def main():
    sock, in_file, out_file = sys.argv[1:4]
    with open(in_file) as f:
        request = json.load(f)
    client = Client(sock)
    client.s.settimeout(10)
    client.hello()
    replies = []
    for one in request if isinstance(request, list) else [request]:
        client.send(one)
        while True:
            reply = client.recv()
            if name_of(reply) != "ViewList":
                break
        replies.append(reply)
    client.close()
    out = replies if isinstance(request, list) else replies[0]
    tmp = out_file + ".tmp"
    with open(tmp, "w") as f:
        f.write(json.dumps(out) + "\n")
    os.rename(tmp, out_file)
    return 0


if __name__ == "__main__":
    sys.exit(main())
