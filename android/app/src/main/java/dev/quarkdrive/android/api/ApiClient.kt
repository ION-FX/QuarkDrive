package dev.quarkdrive.android.api

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.io.IOException
import java.net.URLEncoder
import java.util.concurrent.TimeUnit

/** Any non-2xx response, carrying the server's own error message. */
class ApiException(message: String, val code: Int) : IOException(message)

data class Entry(
    val name: String,
    val path: String,
    val kind: String,
    val size: Long,
    val mtime: Long,
    /** Server-relative thumbnail URL, present for images. */
    val thumbUrl: String?,
) {
    val isDirectory: Boolean get() = kind == "dir"
}

/**
 * Client for the server's file API.
 *
 * The phone does not run the Merkle sync engine — it uploads and downloads
 * whole files, and the server does the chunking. That keeps the client small
 * and makes photo backup work even when the phone only has a moment of
 * connectivity.
 */
class ApiClient(
    private val server: String,
    private val token: String,
) {
    private val binary = "application/octet-stream".toMediaType()
    private val json = "application/json; charset=utf-8".toMediaType()

    private val http: OkHttpClient = OkHttpClient.Builder()
        // Uploads are whole photos or videos over mobile links; the defaults
        // are far too impatient.
        .connectTimeout(30, TimeUnit.SECONDS)
        .readTimeout(5, TimeUnit.MINUTES)
        .writeTimeout(5, TimeUnit.MINUTES)
        .build()

    private fun base() = server.trimEnd('/')

    private fun encode(value: String) = URLEncoder.encode(value, "UTF-8")

    private fun url(vault: String, suffix: String) =
        "${base()}/api/v1/vaults/${encode(vault)}$suffix"

    private fun authed(url: String) =
        Request.Builder().url(url).addHeader("Authorization", "Bearer $token")

    // -------------------------------------------------------------- reading

    fun absoluteUrl(serverRelative: String) = base() + serverRelative

    suspend fun list(vault: String, path: String = ""): List<Entry> = withContext(Dispatchers.IO) {
        val query = if (path.isBlank()) "" else "?path=${encode(path)}"
        val body = execute(authed(url(vault, "/fs$query")).get().build())
        parseEntries(body)
    }

    /**
     * Photo-timeline items have their own shape — {path, taken_at, size,
     * thumb} without a name — so they are mapped into [Entry] here and the
     * UI keeps speaking one type.
     */
    suspend fun timeline(vault: String, limit: Int = 1000): List<Entry> = withContext(Dispatchers.IO) {
        val body = execute(authed(url(vault, "/timeline?limit=$limit")).get().build())
        val array = JSONObject(body).getJSONArray("items")
        (0 until array.length()).map { i ->
            val o = array.getJSONObject(i)
            val path = o.getString("path")
            Entry(
                name = path.substringAfterLast('/'),
                path = path,
                kind = "file",
                size = o.optLong("size", 0L),
                mtime = o.optLong("taken_at", 0L),
                thumbUrl = o.optString("thumb").takeIf { it.isNotBlank() },
            )
        }
    }

    suspend fun download(vault: String, path: String): ByteArray = withContext(Dispatchers.IO) {
        val response = http.newCall(
            authed(url(vault, "/fs/download?path=${encode(path)}")).get().build()
        ).execute()
        response.use {
            if (!it.isSuccessful) throw ApiException(
                it.body?.string()?.let(::errorMessage) ?: it.message, it.code
            )
            it.body?.bytes() ?: throw ApiException("empty response body", it.code)
        }
    }

    // -------------------------------------------------------------- writing

    suspend fun upload(vault: String, path: String, data: ByteArray) = withContext(Dispatchers.IO) {
        execute(
            authed(url(vault, "/fs?path=${encode(path)}"))
                .put(data.toRequestBody(binary, 0, data.size))
                .build()
        )
    }

    suspend fun delete(vault: String, path: String) = withContext(Dispatchers.IO) {
        execute(authed(url(vault, "/fs?path=${encode(path)}")).delete().build())
    }

    /** Rename [from], or move it into another folder by giving a path. */
    suspend fun move(vault: String, from: String, to: String) = withContext(Dispatchers.IO) {
        val query = "/fs/move?from=${encode(from)}&to=${encode(to)}"
        execute(authed(url(vault, query))
            .post(ByteArray(0).toRequestBody(binary, 0, 0))
            .build())
    }

    suspend fun mkdir(vault: String, path: String) = withContext(Dispatchers.IO) {
        execute(authed(url(vault, "/fs/mkdir?path=${encode(path)}")).post(ByteArray(0).toRequestBody(binary, 0, 0)).build())
    }

    suspend fun listVaults(): List<String> = withContext(Dispatchers.IO) {
        val body = execute(authed("${base()}/api/v1/vaults").get().build())
        val array = JSONObject(body).getJSONArray("vaults")
        (0 until array.length()).map { array.getJSONObject(it).getString("name") }
    }

    // ------------------------------------------------------------- plumbing

    private fun execute(request: Request): String {
        http.newCall(request).execute().use { response ->
            val text = response.body?.string().orEmpty()
            if (!response.isSuccessful) {
                throw ApiException(errorMessage(text, response.message), response.code)
            }
            return text
        }
    }

    private fun errorMessage(body: String, fallback: String = ""): String {
        val parsed = runCatching { JSONObject(body).optString("error") }.getOrNull()
        return when {
            !parsed.isNullOrBlank() -> parsed
            fallback.isNotBlank() -> "$fallback: $body"
            else -> body
        }
    }

    private fun parseEntries(body: String): List<Entry> {
        val array = JSONObject(body).getJSONArray("entries")
        return (0 until array.length()).map { i ->
            val o = array.getJSONObject(i)
            Entry(
                name = o.getString("name"),
                path = o.getString("path"),
                kind = o.getString("kind"),
                size = o.optLong("size", 0L),
                mtime = o.optLong("mtime", 0L),
                thumbUrl = o.optString("thumb").takeIf { it.isNotBlank() },
            )
        }
    }

    companion object {
        /** Exchange credentials for a token. Does not need an existing token. */
        suspend fun login(server: String, username: String, password: String): String =
            withContext(Dispatchers.IO) {
                val payload = JSONObject()
                    .put("username", username)
                    .put("password", password)
                    .toString()
                val request = Request.Builder()
                    .url("${server.trimEnd('/')}/api/v1/auth/login")
                    .post(payload.toRequestBody("application/json; charset=utf-8".toMediaType()))
                    .build()
                OkHttpClient().newCall(request).execute().use { response ->
                    val text = response.body?.string().orEmpty()
                    if (!response.isSuccessful) {
                        val message = runCatching { JSONObject(text).optString("error") }.getOrNull()
                        throw ApiException(message?.takeIf { it.isNotBlank() } ?: "login failed", response.code)
                    }
                    JSONObject(text).getString("token")
                }
            }
    }
}
