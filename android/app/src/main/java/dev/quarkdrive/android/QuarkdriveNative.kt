package dev.quarkdrive.android

/**
 * Bridge to the Rust core compiled into `libquarkdrive_ffi.so`.
 *
 * Only the pure, CPU-bound parts of the engine live on this side of the
 * boundary — hashing and content-defined chunking. Anything that needs the
 * network, the filesystem or the Android scheduler stays in Kotlin.
 *
 * The shared library is produced by cargo; see `android/README.md`.
 */
object QuarkdriveNative {
    init {
        System.loadLibrary("quarkdrive_ffi")
    }

    /** BLAKE3 content address of [data], as lowercase hex. */
    private external fun nativeContentHash(data: ByteArray): String?

    /** Lengths of the content-defined chunks [data] splits into. */
    private external fun nativeChunkLengths(data: ByteArray): IntArray?

    /** `[min, avg, max]` chunk sizes in bytes. */
    private external fun nativeChunkLimits(): IntArray?

    /** Version of the compiled core. */
    private external fun nativeVersion(): String?

    /**
     * Content address of [data].
     *
     * This is what makes backup idempotent: the same bytes always produce the
     * same address, so a photo already in the vault — uploaded from any
     * device, under any name — is recognised and skipped.
     */
    fun contentHash(data: ByteArray): String =
        nativeContentHash(data)
            ?: error("Quarkdrive core failed to hash ${data.size} bytes")

    /** Chunk boundaries for [data], as successive chunk lengths. */
    fun chunkLengths(data: ByteArray): IntArray =
        nativeChunkLengths(data)
            ?: error("Quarkdrive core failed to chunk ${data.size} bytes")

    val chunkLimits: IntArray get() = nativeChunkLimits() ?: intArrayOf(0, 0, 0)

    val version: String get() = nativeVersion() ?: "unknown"

    /** True when [candidate] has the same content address as [data]. */
    fun matches(data: ByteArray, candidate: String): Boolean =
        contentHash(data) == candidate
}
