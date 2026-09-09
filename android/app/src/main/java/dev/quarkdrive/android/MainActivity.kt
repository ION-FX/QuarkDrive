package dev.quarkdrive.android

import android.Manifest
import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.MediaStore
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.PhotoLibrary
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import coil.request.ImageRequest
import dev.quarkdrive.android.api.ApiClient
import dev.quarkdrive.android.api.Entry
import dev.quarkdrive.android.data.Config
import dev.quarkdrive.android.data.Settings
import dev.quarkdrive.android.sync.BackupScheduler
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {

    private lateinit var settings: Settings

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        settings = Settings(this)
        requestMediaPermissionIfNeeded()
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                AppRoot(settings)
            }
        }
    }

    /** Reading the camera roll needs READ_MEDIA_IMAGES from Android 13. */
    private fun requestMediaPermissionIfNeeded() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            requestPermissions(arrayOf(Manifest.permission.READ_MEDIA_IMAGES), 0)
        } else {
            requestPermissions(arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE), 0)
        }
    }
}

// ------------------------------------------------------------------ root

@Composable
fun AppRoot(settings: Settings) {
    var config by remember { mutableStateOf<Config?>(null) }
    var loaded by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(Unit) {
        config = settings.config.first()
        loaded = true
    }

    if (!loaded) {
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            CircularProgressIndicator()
        }
        return
    }

    if (config == null) {
        LoginScreen(onSignedIn = { config = it })
    } else {
        MainScreen(
            config = config!!,
            settings = settings,
            onSignOut = {
                scope.launch {
                    settings.clear()
                    config = null
                }
            },
        )
    }
}

// ----------------------------------------------------------------- login

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LoginScreen(onSignedIn: (Config) -> Unit) {
    val context = LocalContext.current
    var server by remember { mutableStateOf("http://") }
    var username by remember { mutableStateOf("") }
    var password by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    Scaffold(topBar = { TopAppBar(title = { Text("Quarkdrive") }) }) { padding ->
        Column(
            modifier = Modifier.padding(padding).padding(20.dp).fillMaxWidth(),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text("Sign in to your vault", style = MaterialTheme.typography.titleMedium)

            OutlinedTextField(
                value = server, onValueChange = { server = it },
                label = { Text("Server") }, singleLine = true, modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = username, onValueChange = { username = it },
                label = { Text("Username") }, singleLine = true, modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = password, onValueChange = { password = it },
                label = { Text("Password") }, singleLine = true,
                visualTransformation = PasswordVisualTransformation(),
                modifier = Modifier.fillMaxWidth(),
            )

            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }

            Button(
                onClick = {
                    busy = true
                    error = null
                    scope.launch {
                        runCatching {
                            val token = ApiClient.login(server, username, password)
                            val vaults = ApiClient(server, token).listVaults()
                            val vault = vaults.firstOrNull()
                                ?: error("this account has no vaults yet")
                            Settings(context).signIn(server, token, vault)
                            Config(server.trimEnd('/'), token, vault, autoBackup = true)
                        }.onSuccess {
                            busy = false
                            onSignedIn(it)
                        }.onFailure {
                            busy = false
                            error = it.message ?: "sign in failed"
                        }
                    }
                },
                enabled = !busy && server.length > 8 && username.isNotBlank() && password.isNotEmpty(),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text(if (busy) "Signing in…" else "Sign in")
            }
        }
    }
}

// ------------------------------------------------------------- main shell

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(config: Config, settings: Settings, onSignOut: () -> Unit) {
    val context = LocalContext.current
    var tab by remember { mutableStateOf(0) }
    val api = remember(config) { ApiClient(config.server, config.token) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(config.vault) },
                actions = {
                    IconButton(onClick = { BackupScheduler.runOnce(context) }) {
                        Icon(Icons.Default.Refresh, contentDescription = "Back up now")
                    }
                },
            )
        },
        bottomBar = {
            NavigationBar {
                NavigationBarItem(
                    selected = tab == 0, onClick = { tab = 0 },
                    icon = { Icon(Icons.Default.Folder, contentDescription = null) },
                    label = { Text("Files") },
                )
                NavigationBarItem(
                    selected = tab == 1, onClick = { tab = 1 },
                    icon = { Icon(Icons.Default.PhotoLibrary, contentDescription = null) },
                    label = { Text("Photos") },
                )
                NavigationBarItem(
                    selected = tab == 2, onClick = { tab = 2 },
                    icon = { Icon(Icons.Default.Add, contentDescription = null) },
                    label = { Text("Backup") },
                )
            }
        },
    ) { padding ->
        when (tab) {
            0 -> FilesScreen(api, config, Modifier.padding(padding))
            1 -> PhotosScreen(api, config, Modifier.padding(padding))
            else -> BackupScreen(settings, config, onSignOut, Modifier.padding(padding))
        }
    }
}

// ----------------------------------------------------------------- files

@Composable
fun FilesScreen(api: ApiClient, config: Config, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    var path by remember { mutableStateOf("") }
    var entries by remember { mutableStateOf<List<Entry>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    suspend fun reload() {
        loading = true
        error = null
        runCatching { api.list(config.vault, path) }
            .onSuccess { entries = it; loading = false }
            .onFailure { error = it.message; loading = false }
    }

    LaunchedEffect(path) { reload() }

    val picker = rememberLauncherForActivityResult(ActivityResultContracts.GetContent()) { uri: Uri? ->
        uri ?: return@rememberLauncherForActivityResult
        scope.launch {
            val bytes = context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
            if (bytes == null) {
                Toast.makeText(context, "Could not read that file", Toast.LENGTH_SHORT).show()
                return@launch
            }
            val remote = if (path.isEmpty()) displayName(context, uri) else "$path/${displayName(context, uri)}"
            runCatching { api.upload(config.vault, remote, bytes) }
                .onSuccess { reload() }
                .onFailure {
                    Toast.makeText(context, "Upload failed: ${it.message}", Toast.LENGTH_LONG).show()
                }
        }
    }

    Scaffold(
        modifier = modifier,
        floatingActionButton = {
            FloatingActionButton(onClick = { picker.launch("*/*") }) {
                Icon(Icons.Default.Add, contentDescription = "Upload a file")
            }
        },
    ) { padding ->
        Column(Modifier.padding(padding)) {
            if (path.isNotEmpty()) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    IconButton(onClick = { path = path.substringBeforeLast('/', "") }) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Up a folder")
                    }
                    Text(path, style = MaterialTheme.typography.bodyMedium)
                }
            }

            when {
                loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator()
                }
                error != null -> Text(
                    error!!, color = MaterialTheme.colorScheme.error,
                    modifier = Modifier.padding(16.dp),
                )
                entries.isEmpty() -> Text("Nothing here yet.", modifier = Modifier.padding(16.dp))
                else -> LazyColumn {
                    items(entries, key = { it.path }) { entry ->
                        FileRow(
                            entry = entry,
                            onClick = {
                                if (entry.isDirectory) path = entry.path
                                else scope.launch {
                                    runCatching { api.download(config.vault, entry.path) }
                                        .onSuccess { bytes ->
                                            val saved = saveToDownloads(context, entry.name, bytes)
                                            Toast.makeText(
                                                context,
                                                if (saved != null) "Saved to Downloads"
                                                else "Could not save the file",
                                                Toast.LENGTH_SHORT,
                                            ).show()
                                        }
                                        .onFailure {
                                            Toast.makeText(
                                                context, "Download failed: ${it.message}",
                                                Toast.LENGTH_LONG,
                                            ).show()
                                        }
                                }
                            },
                            onDelete = {
                                scope.launch {
                                    runCatching { api.delete(config.vault, entry.path) }
                                        .onSuccess { reload() }
                                        .onFailure {
                                            Toast.makeText(
                                                context, "Delete failed: ${it.message}",
                                                Toast.LENGTH_LONG,
                                            ).show()
                                        }
                                }
                            },
                        )
                    }
                }
            }
        }
    }
}

@Composable
fun FileRow(entry: Entry, onClick: () -> Unit, onDelete: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(if (entry.isDirectory) "📁" else "📄", modifier = Modifier.padding(end = 12.dp))
        Text(entry.name, modifier = Modifier.weight(1f), maxLines = 1)
        if (!entry.isDirectory) {
            Text(humanSize(entry.size), style = MaterialTheme.typography.bodySmall)
        }
        IconButton(onClick = onDelete) {
            Icon(Icons.Default.Delete, contentDescription = "Delete ${entry.name}")
        }
    }
}

/**
 * The real file name behind a content URI.
 *
 * The system picker's URIs do not carry the file name in their path (a
 * recents result looks like `content://…/document/1000000018`), so it has to
 * come from the provider — otherwise uploads would land in the vault under
 * the provider's opaque id.
 */
fun displayName(context: Context, uri: Uri): String {
    if (uri.scheme == "content") {
        context.contentResolver.query(
            uri, arrayOf(android.provider.OpenableColumns.DISPLAY_NAME), null, null, null,
        )?.use { cursor ->
            if (cursor.moveToFirst()) {
                cursor.getString(0)?.takeIf { it.isNotBlank() }?.let { return it }
            }
        }
    }
    return uri.lastPathSegment?.substringAfterLast('/') ?: "upload.bin"
}

/** Write a downloaded file to the shared Downloads folder. */
fun saveToDownloads(context: Context, name: String, bytes: ByteArray): Uri? {    val values = ContentValues().apply {
        put(MediaStore.Downloads.DISPLAY_NAME, name)
        put(MediaStore.Downloads.MIME_TYPE, "application/octet-stream")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            put(MediaStore.Downloads.IS_PENDING, 1)
        }
    }
    val uri = context.contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
        ?: return null
    context.contentResolver.openOutputStream(uri)?.use { it.write(bytes) }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
        val done = ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }
        context.contentResolver.update(uri, done, null, null)
    }
    return uri
}

// ---------------------------------------------------------------- photos

@Composable
fun PhotosScreen(api: ApiClient, config: Config, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    var items by remember { mutableStateOf<List<Entry>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(config) {
        runCatching { api.timeline(config.vault) }
            .onSuccess { items = it; loading = false }
            .onFailure { error = it.message; loading = false }
    }

    Box(modifier.fillMaxSize()) {
        when {
            loading -> CircularProgressIndicator(Modifier.align(Alignment.Center))
            error != null -> Text(
                error!!, Modifier.padding(16.dp),
                color = MaterialTheme.colorScheme.error,
            )
            items.isEmpty() -> Text("No photos in this vault yet.", Modifier.padding(16.dp))
            else -> LazyVerticalGrid(
                columns = GridCells.Adaptive(minSize = 110.dp),
                contentPadding = PaddingValues(8.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                items(items, key = { it.path }) { entry ->
                    val url = entry.thumbUrl?.let { api.absoluteUrl(it) }
                    AsyncImage(
                        model = ImageRequest.Builder(context)
                            .data(url)
                            // Thumbnails are authenticated, and an image load
                            // cannot attach a header on its own.
                            .addHeader("Authorization", "Bearer ${config.token}")
                            .crossfade(true)
                            .build(),
                        contentDescription = entry.name,
                        contentScale = ContentScale.Crop,
                        modifier = Modifier.size(110.dp),
                    )
                }
            }
        }
    }
}

// ---------------------------------------------------------------- backup

@Composable
fun BackupScreen(
    settings: Settings,
    config: Config,
    onSignOut: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    var enabled by remember { mutableStateOf(config.autoBackup) }
    var backedUp by remember { mutableStateOf(0) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(Unit) {
        backedUp = settings.backedUpHashes.first().size
    }

    Column(modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(14.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Back up camera photos", modifier = Modifier.weight(1f))
            Switch(checked = enabled, onCheckedChange = { on ->
                enabled = on
                scope.launch {
                    settings.setAutoBackup(on)
                    if (on) BackupScheduler.enable(context) else BackupScheduler.disable(context)
                }
            })
        }

        Text(
            "New photos upload automatically when the device has connectivity. " +
                "$backedUp item(s) already backed up.",
            style = MaterialTheme.typography.bodySmall,
        )

        Text(
            "Sync engine: Quarkdrive core ${QuarkdriveNative.version}",
            style = MaterialTheme.typography.bodySmall,
        )

        Button(onClick = onSignOut) { Text("Sign out") }
    }
}

// --------------------------------------------------------------- helpers

fun humanSize(bytes: Long): String {
    if (bytes < 1024) return "$bytes B"
    val units = arrayOf("KiB", "MiB", "GiB", "TiB")
    var value = bytes / 1024.0
    var unit = 0
    while (value >= 1024 && unit < units.size - 1) {
        value /= 1024
        unit += 1
    }
    return String.format("%.1f %s", value, units[unit])
}
