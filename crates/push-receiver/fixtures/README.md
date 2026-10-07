# Push-receiver fixtures

These fixtures are checked-in characterization data. Live FCM/MCS capture is
not possible in CI because it requires account credentials, so non-crypto
fixtures are derived from the pinned reference implementation and its wire
schemas rather than copied from a live account.

## Common provenance

- Reference repository: `kunihir0/push-receiver`, `master` at
  `3b5b741c7ef52c2da26414bad2cffbf778644fd`.
- MCS schema: `crates/push-receiver/proto/mcs.proto` in this repository,
  matching the reference `proto/mcs.proto`; framing uses MCS version 41,
  tag + varint length + protobuf payload.
- Check-in schema: `crates/push-receiver/proto/android_checkin.proto` and
  `checkin.proto`, matching the corresponding reference proto files.
- MCS field meanings and the login/heartbeat/data flows follow Chromium's
  MCS client documentation (`chromium/src/google_apis/gcm/engine/mcs.proto`
  and `mcs_client.cc`, pinned by the reference commit above).
- Values are deterministic characterization values (not credentials):
  Android ID `0x12345678`, security token `0x87654321`, login resource
  `android-12345678`, category `cat`, sender `fcm`, persistent ID `pid`, and
  raw payload `hello`.

## File inventory

| File | Source and encoded fields |
| --- | --- |
| `login_response.hex` | The unframed `LoginResponse` protobuf corresponding to the server frame: id `server` and heartbeat interval `300000` ms. |
| `server_login_response_frame.hex` | Reference MCS `LoginResponse` framing (`mcs.proto`, tag 3), with server id `server`; the independent unframed fixture carries the heartbeat configuration. |
| `data_message_frame.hex` | Reference MCS `DataMessageStanza` framing (version 41/tag 8), with sender `fcm`, category `cat`, persistent ID `pid`, raw data `hello`, and immediate ACK set. |
| `data_message.hex` | The unframed `DataMessageStanza` protobuf corresponding to `data_message_frame.hex`; fields are listed above. |
| `heartbeat_frames.hex` | Chromium MCS heartbeat ping/ack frames (tags 0 and 1), stream id `1`, last received stream id `2`. |
| `checkin_response.hex` | Protobuf `AndroidCheckinResponse` from the pinned check-in schema: accepted statistics, Android ID `0x12345678`, and security token `0x87654321`. |
| `registration_response.txt` | Legacy `register3` response shape from Chromium's registration flow: `token=<non-empty registration token>`. The token is synthetic and contains no credential. |
| `truncated_frame.hex` | MCS version 41/data tag with a declared payload longer than the available bytes; derived from the reference decoder's EOF path. |
| `malformed_varint.hex` | MCS data tag with an overlong/overflowing payload-length varint; derived from the reference framing rules. |

## Web Push crypto provenance

- `web_push_aesgcm.hex`, `web_push_malformed_padding.hex`, and
  `web_push_all_padding.hex` are the RFC 8188 `aesgcm` record vector and two
  authenticated mutations. The key material, salt, peer public key, and
  expected plaintext are documented in `crypto.rs`; the valid plaintext is
  `I am the walrus`.
- The ECDH context and key derivation are the RFC 8291 P-256/Web Push
  construction (the RFC 8188 content-coding record format), and the AES-GCM
  primitive is checked independently against the NIST AES-128-GCM known-answer
  vector in `crypto.rs`.

All fixture consumers parse the files independently in unit tests; no fixture
is used as an implicit credential or network endpoint.
