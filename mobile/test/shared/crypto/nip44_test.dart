import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:buzz/shared/crypto/ecdh.dart';
import 'package:buzz/shared/crypto/hkdf.dart';
import 'package:buzz/shared/crypto/nip44.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pointycastle/api.dart';
import 'package:pointycastle/digests/sha256.dart';
import 'package:pointycastle/macs/hmac.dart';
import 'package:pointycastle/stream/chacha7539.dart';

/// The official NIP-44 v2 vectors, verbatim from the `nostr-protocol/nips`
/// repository (the same file `rust-nostr` compiles into its own test suite).
///
/// This file is load-bearing in eight places on mobile — read state, channel
/// stars, mutes, sections, reminders, pairing and agent observer frames — and
/// until now had no tests at all. Two of those paths are *cross-party*
/// (pairing, observer frames), which is exactly where a divergence between
/// this implementation and the Rust one stops being academic: an event the
/// relay and desktop reject is one mobile would happily read.
Map<String, dynamic> _loadVectors() {
  final file = File('test/shared/crypto/nip44.vectors.json');
  return (jsonDecode(file.readAsStringSync()) as Map<String, dynamic>)['v2']
      as Map<String, dynamic>;
}

String _hex(Uint8List bytes) =>
    bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();

Uint8List _hmacSha256(Uint8List key, Uint8List data) {
  final hmac = HMac(SHA256Digest(), 64);
  hmac.init(KeyParameter(key));
  hmac.update(data, 0, data.length);
  final out = Uint8List(32);
  hmac.doFinal(out, 0);
  return out;
}

Uint8List _chacha20(Uint8List key, Uint8List iv, Uint8List data) {
  final cipher = ChaCha7539Engine();
  cipher.init(false, ParametersWithIV(KeyParameter(key), iv));
  return cipher.process(data);
}

Uint8List _concat(List<Uint8List> parts) {
  final out = Uint8List(parts.fold<int>(0, (s, p) => s + p.length));
  var offset = 0;
  for (final part in parts) {
    out.setRange(offset, offset + part.length, part);
    offset += part.length;
  }
  return out;
}

/// Forge a payload that carries a **valid** MAC but a body of `paddedLen`
/// bytes, declaring `declaredLen` bytes of plaintext.
///
/// A valid MAC is the whole point: it puts the payload past the HMAC gate so
/// the test isolates the length and padding checks rather than passing for
/// the same reason a random string would.
String _forgePayload({
  required Uint8List conversationKey,
  required int paddedLen,
  required int declaredLen,
}) {
  final nonce = Uint8List(32)..[31] = 0x07;
  final messageKeys = hkdfExpand(conversationKey, nonce, 76);
  final chachaKey = Uint8List.sublistView(messageKeys, 0, 32);
  final chachaIv = Uint8List.sublistView(messageKeys, 32, 44);
  final hmacKey = Uint8List.sublistView(messageKeys, 44, 76);

  final padded = Uint8List(paddedLen);
  padded[0] = (declaredLen >> 8) & 0xFF;
  padded[1] = declaredLen & 0xFF;
  for (var i = 0; i < declaredLen; i++) {
    padded[2 + i] = 0x78; // 'x'
  }

  final ciphertext = _chacha20(chachaKey, chachaIv, padded);
  final mac = _hmacSha256(hmacKey, _concat([nonce, ciphertext]));
  return base64.encode(
    _concat([
      Uint8List.fromList([0x02]),
      nonce,
      ciphertext,
      mac,
    ]),
  );
}

void main() {
  final vectors = _loadVectors();
  final valid = vectors['valid'] as Map<String, dynamic>;
  final invalid = vectors['invalid'] as Map<String, dynamic>;

  group('NIP-44 v2 — official vectors, valid', () {
    test('conversation keys derive from sec1 + pub2', () {
      final cases = valid['get_conversation_key'] as List<dynamic>;
      expect(cases, isNotEmpty);
      for (final c in cases.cast<Map<String, dynamic>>()) {
        final got = getConversationKey(
          c['sec1'] as String,
          c['pub2'] as String,
        );
        expect(_hex(got), c['conversation_key'], reason: c['note'] as String?);
      }
    });

    test('reference ciphertexts decrypt to their plaintext', () {
      final cases = valid['encrypt_decrypt'] as List<dynamic>;
      expect(cases, isNotEmpty);
      for (final c in cases.cast<Map<String, dynamic>>()) {
        final key = hexToBytes(c['conversation_key'] as String);
        expect(
          nip44Decrypt(key, c['ciphertext'] as String),
          c['plaintext'],
          reason: 'plaintext ${(c['plaintext'] as String).length} chars',
        );
      }
    });

    /// Our own output must byte-match the reference, not merely round-trip
    /// with itself. A self-consistent-but-wrong padding or assembly order
    /// survives a round-trip test and fails here.
    test('our ciphertext byte-matches the reference for a fixed nonce', () {
      final cases = valid['encrypt_decrypt'] as List<dynamic>;
      for (final c in cases.cast<Map<String, dynamic>>()) {
        final key = hexToBytes(c['conversation_key'] as String);
        final got = nip44EncryptWithNonce(
          key,
          c['plaintext'] as String,
          hexToBytes(c['nonce'] as String),
        );
        expect(got, c['ciphertext']);
      }
    });

    /// `_calcPaddedLen` is private, so this reaches it through the only door
    /// the app uses: the length of what encrypt actually emits.
    /// payload = version(1) + nonce(32) + ciphertext + mac(32), and
    /// ciphertext length == padded length == 2 + calc_padded_len(n).
    test('padded lengths match the reference table', () {
      final pairs = valid['calc_padded_len'] as List<dynamic>;
      final key = Uint8List(32)..[0] = 0x01;
      var checked = 0;
      for (final pair in pairs.cast<List<dynamic>>()) {
        final unpadded = pair[0] as int;
        final expectedPadded = pair[1] as int;
        // 65536 exceeds the 65535-byte plaintext ceiling, so it is not
        // reachable through the public API — the encrypt-length vectors
        // below pin that refusal instead.
        if (unpadded > 65535) continue;
        final payload = base64.decode(nip44Encrypt(key, 'a' * unpadded));
        expect(
          payload.length - 65,
          2 + expectedPadded,
          reason: 'unpadded $unpadded should pad to $expectedPadded',
        );
        checked++;
      }
      expect(checked, 23, reason: 'all but the unreachable 65536 pair');
    });
  });

  group('NIP-44 v2 — official vectors, invalid', () {
    test('conversation keys off the curve are refused', () {
      final cases = invalid['get_conversation_key'] as List<dynamic>;
      expect(cases, isNotEmpty);
      for (final c in cases.cast<Map<String, dynamic>>()) {
        expect(
          () => getConversationKey(c['sec1'] as String, c['pub2'] as String),
          throwsA(anything),
          reason: c['note'] as String?,
        );
      }
    });

    test('plaintexts outside 1..65535 bytes are refused', () {
      final lengths = invalid['encrypt_msg_lengths'] as List<dynamic>;
      final key = Uint8List(32)..[0] = 0x01;
      for (final len in lengths.cast<int>()) {
        expect(
          () => nip44Encrypt(key, 'a' * len),
          throwsA(isA<ArgumentError>()),
          reason: 'plaintext of $len bytes',
        );
      }
    });

    test('payloads shorter than a minimal frame are refused', () {
      final lengths = invalid['decrypt_msg_lengths'] as List<dynamic>;
      final key = Uint8List(32)..[0] = 0x01;
      for (final len in lengths.cast<int>()) {
        // Byte 0 is a *valid* version, so a rejection here is the length
        // check doing its job and not the version check standing in for it.
        final payload = Uint8List(len);
        if (len > 0) payload[0] = 0x02;
        expect(
          () => nip44Decrypt(key, base64.encode(payload)),
          throwsA(isA<FormatException>()),
          reason: 'payload of $len bytes',
        );
      }
    });

    /// Five vectors: two with a corrupted MAC, three with padding that is
    /// well-formed enough to slice but not canonical. All five are 99 or 132
    /// bytes — long enough that no length check can reach them.
    test('malformed frames are refused', () {
      final cases = invalid['decrypt'] as List<dynamic>;
      expect(cases, isNotEmpty);
      for (final c in cases.cast<Map<String, dynamic>>()) {
        final key = hexToBytes(c['conversation_key'] as String);
        expect(
          () => nip44Decrypt(key, c['ciphertext'] as String),
          throwsA(isA<FormatException>()),
          reason: c['note'] as String?,
        );
      }
    });
  });

  /// The official `decrypt_msg_lengths` vectors stop at 64 bytes, so nothing
  /// upstream exercises the boundary between "too short" and "minimal valid".
  /// A minimal frame is version(1) + nonce(32) + ciphertext(34) + mac(32) =
  /// 99, because the smallest padded plaintext is a 2-byte length prefix plus
  /// a 32-byte minimum pad — not 32.
  group('NIP-44 v2 — the boundary the official vectors miss', () {
    final key = Uint8List(32)..[0] = 0x01;

    // Asserting *which* check fires, not merely that one does. The canonical
    // padding rule below would reject these frames too, so a bare
    // `throwsA(FormatException)` here would stay green with the length floor
    // put back to 97 — pinning nothing. The message is the only thing that
    // tells the two checks apart.
    for (final paddedLen in [32, 33]) {
      test('a MAC-valid frame with a $paddedLen-byte body is too short', () {
        final payload = _forgePayload(
          conversationKey: key,
          paddedLen: paddedLen,
          declaredLen: 1,
        );
        expect(base64.decode(payload).length, 65 + paddedLen);
        expect(
          () => nip44Decrypt(key, payload),
          throwsA(
            isA<FormatException>().having(
              (e) => e.message,
              'message',
              contains('too short'),
            ),
          ),
        );
      });
    }

    test('a MAC-valid frame with non-canonical padding is refused', () {
      // 34 bytes of body declaring 1 byte of plaintext is long enough to
      // pass every length check, but a 1-byte plaintext pads to 32, so a
      // canonical frame would carry exactly 2 + 32 = 34 bytes. Declare a
      // plaintext that pads to something else and the frame is not canonical
      // even though it slices cleanly.
      final payload = _forgePayload(
        conversationKey: key,
        paddedLen: 2 + 64,
        declaredLen: 1,
      );
      expect(base64.decode(payload).length, greaterThan(99));
      expect(
        () => nip44Decrypt(key, payload),
        throwsA(
          isA<FormatException>().having(
            (e) => e.message,
            'message',
            contains('Non-canonical'),
          ),
        ),
      );
    });

    test('a minimal canonical frame still round-trips', () {
      final payload = nip44Encrypt(key, 'x');
      expect(base64.decode(payload).length, 99);
      expect(nip44Decrypt(key, payload), 'x');
    });
  });
}
