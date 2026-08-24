# terminal endpoint validation

The Luvus control endpoint is a local-user capability that can type into and
close terminals. A consumer must validate every discovery result before use.

On Unix:

1. Require an absolute discovery-supplied socket address.
2. Inspect every path component without following symlinks.
3. Reject symlinks, non-directory parents, and non-socket endpoints.
4. Require the Luvus state/session directory and socket to belong to the current
   effective user. Require directory mode `0700` and socket mode `0600`.
5. Reject group- or world-writable user-owned ancestors.
6. Canonicalize after the no-follow checks and require the result to match.
7. Record socket device, inode, and change timestamp, repeat validation before
   each connection, then negotiate the protocol before trusting endpoint
   metadata. The timestamp prevents an immediately reused inode from making a
   replacement endpoint look unchanged.

The only world-writable ancestor exception is a long-path alias below the native
temporary root: `/tmp` on Linux and `/private/tmp` on macOS. That root must be a
non-symlink, root-owned directory with mode `01777`; its `luvus-<uid>` child
must belong to the effective user with mode `0700`; and the socket must belong
to that user with mode `0600`.

On Windows:

1. Require the discovery-supplied `windows_named_pipe` transport and an address
   beginning with `\\.\pipe\`. Never accept a remote host component.
2. Use the exact discovery address. Consumers must not reproduce Luvus's
   internal pipe-name derivation.
3. The server creates the pipe with remote clients rejected and a protected
   DACL granting full access only to the pipe owner and LocalSystem.
4. After connecting, obtain the named-pipe server PID and require its process
   token user SID to equal the consumer's current process user SID.
5. Negotiate protocol 1.0 and retain the returned random `server_generation`.
   Supply that generation with every identity-sensitive request and reject a
   changed generation after reconnecting.

Pipe names are not security boundaries. A predictable or squatted name is safe
only when the DACL and same-user server checks pass.
