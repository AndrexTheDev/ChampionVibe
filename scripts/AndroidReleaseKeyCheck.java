import java.io.File;
import java.security.KeyStore;
import java.security.MessageDigest;
import java.security.PrivateKey;
import java.security.Signature;
import java.security.cert.X509Certificate;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.Locale;

/** Check the same key entry apksigner will use, without any APK. */
class AndroidReleaseKeyCheck {
    public static void main(String[] args) {
        char[] storePassword = System.getenv("ANDROID_RELEASE_STORE_PASSWORD").toCharArray();
        char[] keyPassword = System.getenv("ANDROID_RELEASE_KEY_PASSWORD").toCharArray();
        boolean valid = false;
        try {
            check(args, storePassword, keyPassword);
            valid = true;
        } catch (Exception e) {
            // Avoid dumping provider exceptions or credential material into an Actions log.
            System.err.println("Could not validate the Android keystore, key or certificate.");
        } finally {
            Arrays.fill(storePassword, '\0');
            Arrays.fill(keyPassword, '\0');
        }
        if (!valid) {
            System.exit(1);
        }
    }

    private static void check(String[] args, char[] storePassword, char[] keyPassword) throws Exception {
        var store = KeyStore.getInstance(new File(args[0]), storePassword);
        String alias = args[1];
        if (!(store.getKey(alias, keyPassword) instanceof PrivateKey key)
                || !(store.getCertificate(alias) instanceof X509Certificate certificate)) {
            throw new IllegalArgumentException("Expected a private signing key with an X.509 certificate");
        }
        certificate.checkValidity();
        byte[] fingerprint = MessageDigest.getInstance("SHA-256").digest(certificate.getEncoded());
        if (!MessageDigest.isEqual(fingerprint, HexFormat.of().parseHex(args[2]))) {
            throw new IllegalArgumentException("Certificate mismatch");
        }
        String algorithm = switch (key.getAlgorithm().toUpperCase(Locale.ROOT)) {
            case "RSA" -> "SHA256withRSA";
            case "EC" -> "SHA256withECDSA";
            case "DSA" -> "SHA256withDSA";
            default -> throw new IllegalArgumentException("Unsupported APK key algorithm");
        };
        byte[] probe = "ChampionVibe Android release key check".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        Signature signature = Signature.getInstance(algorithm);
        signature.initSign(key);
        signature.update(probe);
        byte[] signed = signature.sign();
        signature.initVerify(certificate.getPublicKey());
        signature.update(probe);
        if (!signature.verify(signed)) {
            throw new IllegalArgumentException("Private key does not match the certificate");
        }
    }
}
