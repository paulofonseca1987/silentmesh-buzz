import 'dart:convert';
import 'dart:typed_data';

import 'package:meta/meta.dart';
import 'package:pointycastle/api.dart';
import 'package:pointycastle/digests/sha256.dart';
import 'package:pointycastle/macs/hmac.dart';
import 'package:pointycastle/stream/chacha7539.dart';

import 'ecdh.dart';
import 'hkdf.dart';

/// NIP-44 v2 conversation key derivation.
///
/// conversation_key = HKDF-Extract(salt="nip44-v2", ikm=ecdh_shared_secret)
Uint8List getConversationKey(String senderPrivHex, String receiverPubHex) {
  final shared = ecdhSharedSecret(senderPrivHex, receiverPubHex);
  final salt = utf8.encode('nip44-v2');
  return hkdfExtract(Uint8List.fromList(salt), shared);
}

/// NIP-44 v2 encrypt.
///
/// Returns base64-encoded payload: version(1) || nonce(32) || ciphertext || mac(32)
String nip44Encrypt(Uint8List conversationKey, String plaintext) =>
    nip44EncryptWithNonce(conversationKey, plaintext, secureRandomBytes(32));

/// [nip44Encrypt] with the nonce supplied rather than generated.
///
/// Exists so the official NIP-44 vectors — which pin an exact ciphertext for
/// an exact nonce — can be asserted byte-for-byte. A round-trip test cannot
/// do that: padding or assembly that is wrong but self-consistent round-trips
/// happily and still fails to interoperate with the relay and desktop.
///
/// Production code must never call this. Reusing a nonce under one
/// conversation key is catastrophic for a stream cipher — the keystream
/// repeats and XORing two ciphertexts cancels it out entirely.
@visibleForTesting
String nip44EncryptWithNonce(
  Uint8List conversationKey,
  String plaintext,
  Uint8List nonce,
) {
  final plaintextBytes = utf8.encode(plaintext);
  if (plaintextBytes.isEmpty || plaintextBytes.length > 65535) {
    throw ArgumentError('Plaintext must be 1-65535 bytes');
  }
  if (nonce.length != 32) {
    throw ArgumentError('Nonce must be 32 bytes, got ${nonce.length}');
  }

  final padded = _pad(Uint8List.fromList(plaintextBytes));

  // Derive message keys: chacha_key(32) + chacha_nonce(12) + hmac_key(32) = 76
  final messageKeys = hkdfExpand(conversationKey, nonce, 76);
  final chachaKey = Uint8List.sublistView(messageKeys, 0, 32);
  final chachaNonce = Uint8List.sublistView(messageKeys, 32, 44);
  final hmacKey = Uint8List.sublistView(messageKeys, 44, 76);

  final ciphertext = _chacha20(chachaKey, chachaNonce, padded);

  // HMAC-SHA256 over nonce || ciphertext.
  final mac = _hmacSha256(hmacKey, _concat([nonce, ciphertext]));

  // Assemble: version(0x02) || nonce(32) || ciphertext || mac(32)
  final payload = _concat([
    Uint8List.fromList([0x02]),
    nonce,
    ciphertext,
    mac,
  ]);

  return base64.encode(payload);
}

/// NIP-44 v2 decrypt.
///
/// Takes base64-encoded payload, returns plaintext string.
String nip44Decrypt(Uint8List conversationKey, String payloadBase64) {
  final payload = base64.decode(payloadBase64);

  // Minimum: version(1) + nonce(32) + min_ciphertext(34) + mac(32) = 99.
  //
  // The ciphertext floor is 34, not 32: ChaCha20 is a stream cipher, so the
  // ciphertext is exactly as long as the padded plaintext, and the smallest
  // padded plaintext is a 2-byte big-endian length prefix followed by the
  // 32-byte minimum pad. Forgetting the prefix gives 97, which accepts two
  // payload lengths the relay and every other client reject.
  if (payload.length < 99) {
    throw FormatException('NIP-44 payload too short: ${payload.length}');
  }

  if (payload[0] != 0x02) {
    throw FormatException('NIP-44 unsupported version: ${payload[0]}');
  }

  final nonce = Uint8List.sublistView(payload, 1, 33);
  final ciphertext = Uint8List.sublistView(payload, 33, payload.length - 32);
  final receivedMac = Uint8List.sublistView(payload, payload.length - 32);

  final messageKeys = hkdfExpand(conversationKey, nonce, 76);
  final chachaKey = Uint8List.sublistView(messageKeys, 0, 32);
  final chachaNonce = Uint8List.sublistView(messageKeys, 32, 44);
  final hmacKey = Uint8List.sublistView(messageKeys, 44, 76);

  // Verify HMAC.
  final expectedMac = _hmacSha256(hmacKey, _concat([nonce, ciphertext]));
  if (!constantTimeEquals(receivedMac, expectedMac)) {
    throw FormatException('NIP-44 HMAC verification failed');
  }

  final padded = _chacha20(chachaKey, chachaNonce, ciphertext);

  return _unpad(padded);
}

// ── Padding ─────────────────────────────────────────────────────────────────

int _calcPaddedLen(int unpaddedLen) {
  if (unpaddedLen <= 0) throw ArgumentError('Length must be > 0');
  if (unpaddedLen <= 32) return 32;
  final nextPower = 1 << (_log2(unpaddedLen - 1) + 1);
  final chunk = nextPower <= 256 ? 32 : nextPower ~/ 8;
  return chunk * (((unpaddedLen - 1) ~/ chunk) + 1);
}

int _log2(int x) {
  var result = 0;
  while ((1 << (result + 1)) <= x) {
    result++;
  }
  return result;
}

Uint8List _pad(Uint8List plaintext) {
  final len = plaintext.length;
  final paddedLen = _calcPaddedLen(len);
  // Result: [2-byte BE length][plaintext][zero padding]
  final result = Uint8List(2 + paddedLen);
  result[0] = (len >> 8) & 0xFF;
  result[1] = len & 0xFF;
  result.setRange(2, 2 + len, plaintext);
  return result;
}

String _unpad(Uint8List padded) {
  if (padded.length < 2) throw FormatException('Padded data too short');
  final len = (padded[0] << 8) | padded[1];
  if (len == 0 || 2 + len > padded.length) {
    throw FormatException('Invalid padding length: $len');
  }
  // The declared length must account for the whole buffer under the padding
  // rule — not merely fit inside it. Without this, a sender can pad to any
  // length it likes and stash arbitrary bytes after the plaintext, and the
  // frame still decrypts here while the relay and desktop refuse it. That is
  // a silent divergence: the same event reads as text on mobile and as an
  // error everywhere else.
  if (padded.length != 2 + _calcPaddedLen(len)) {
    throw FormatException(
      'Non-canonical NIP-44 padding: ${padded.length} bytes for a '
      '$len-byte plaintext',
    );
  }
  return utf8.decode(padded.sublist(2, 2 + len));
}

// ── ChaCha20 (IETF, 12-byte nonce) ─────────────────────────────────────────

Uint8List _chacha20(Uint8List key, Uint8List nonce, Uint8List data) {
  final cipher = ChaCha7539Engine();
  cipher.init(false, ParametersWithIV(KeyParameter(key), nonce));
  return cipher.process(data);
}

// ── HMAC-SHA256 ─────────────────────────────────────────────────────────────

Uint8List _hmacSha256(Uint8List key, Uint8List data) {
  final hmac = HMac(SHA256Digest(), 64);
  hmac.init(KeyParameter(key));
  hmac.update(data, 0, data.length);
  final out = Uint8List(32);
  hmac.doFinal(out, 0);
  return out;
}

// ── Helpers ─────────────────────────────────────────────────────────────────

Uint8List _concat(List<Uint8List> parts) {
  final totalLen = parts.fold<int>(0, (sum, p) => sum + p.length);
  final result = Uint8List(totalLen);
  var offset = 0;
  for (final part in parts) {
    result.setRange(offset, offset + part.length, part);
    offset += part.length;
  }
  return result;
}

/// Constant-time byte comparison to prevent timing side-channels.
bool constantTimeEquals(Uint8List a, Uint8List b) {
  if (a.length != b.length) return false;
  var result = 0;
  for (var i = 0; i < a.length; i++) {
    result |= a[i] ^ b[i];
  }
  return result == 0;
}
