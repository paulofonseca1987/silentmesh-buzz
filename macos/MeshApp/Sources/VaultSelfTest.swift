import Foundation
import MeshProtocol
import MeshVault

/// A Secure Enclave self-test, run in the signed app because that is the
/// only place the enclave will answer.
///
/// `swift test` binaries are ad-hoc signed, so the enclave refuses them —
/// which means a vault suite that "passes" in SwiftPM would only ever be
/// testing the software path. This runs the hardware path for real and
/// reports what the OS actually said, including the entitlement errors
/// that are the usual reason it fails.
///
///     MESH_VAULT_SELFTEST=1                # no prompt: seal + unlock
///     MESH_VAULT_SELFTEST=presence         # one process, prompts once
///     MESH_VAULT_SELFTEST=presence-seal    # seal, then exit
///     MESH_VAULT_SELFTEST=presence-unlock  # a FRESH process unlocks it
enum VaultSelfTest {
    static func mode(
        _ environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> String? {
        environment["MESH_VAULT_SELFTEST"]
    }

    static func run(mode: String) -> Int32 {
        var failures = 0
        func report(_ label: String, _ outcome: String) {
            FileHandle.standardError.write(Data("vault: \(label): \(outcome)\n".utf8))
        }

        let available = MeshVault.isSecureEnclaveAvailable()
        report("enclave available", available ? "yes" : "NO")
        if !available { failures += 1 }

        // Seal and unlock in ONE process proves the wrapping works, not that
        // the vault is locked at rest: macOS can treat a key's creator as
        // already authenticated, so a same-process round trip may never
        // prompt. The claim worth testing — "the app relaunches locked" — is
        // cross-process, so `presence-seal` and `presence-unlock` run as two
        // separate launches against a fixed vault name.
        let phase = mode
        let requiresPresence = phase.hasPrefix("presence")
        let name = phase.hasPrefix("presence") ? "selftest-presence" : "selftest-\(UUID().uuidString.prefix(8))"
        let vault = MeshVault(
            name: name,
            protection: requiresPresence ? .secureEnclaveWithUserPresence : .secureEnclave)
        let secret = String(repeating: "5c", count: 32)
        let expected = try? MeshKeys(privateKeyHex: secret)

        if phase != "presence-unlock" {
            do {
                try vault.seal(identityHex: secret)
                report("seal", "ok")
            } catch {
                report("seal", "FAILED — \(error)")
                return 1
            }
            if phase == "presence-seal" {
                report("result", "SEALED — now run MESH_VAULT_SELFTEST=presence-unlock")
                return 0
            }
        }

        do {
            let started = Date()
            let unlocked = try vault.unlock(
                reason: "Unlock your Silent Mesh identity (self-test)")
            let elapsed = Date().timeIntervalSince(started)
            let matches = unlocked.publicKeyHex == expected?.publicKeyHex
            // How long the unlock took distinguishes a real prompt from a
            // silent grant: a human touching a sensor takes seconds, a
            // cached authorisation returns immediately.
            report(
                "unlock",
                matches
                    ? "ok — identity round-tripped in \(String(format: "%.1f", elapsed))s"
                    : "FAILED — wrong identity")
            if !matches { failures += 1 }
        } catch {
            report("unlock", "FAILED — \(error)")
            failures += 1
        }

        do {
            try vault.destroy()
            report("destroy", "ok")
        } catch {
            report("destroy", "FAILED — \(error)")
            failures += 1
        }

        report("result", failures == 0 ? "PASS" : "\(failures) FAILURE(S)")
        return failures == 0 ? 0 : 1
    }
}
