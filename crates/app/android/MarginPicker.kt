// Margin's pickers and held folders on Android (Storage Access Framework). build.rs
// copies this file next to wry's Kotlin files in the Gradle project dx generates; Rust
// calls it through JNI (src/picker/android.rs, src/folders/android.rs).

package {{package}}

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import java.io.File
import java.io.FileNotFoundException
import java.util.concurrent.atomic.AtomicInteger

/**
 * One pick: registers a one-off activity result callback, launches the system picker and
 * hands what was picked to Rust through [onPicked] (registered from Rust with
 * RegisterNatives).
 */
class MarginPicker private constructor(private val token: Long) {
    /** `uris`: the picked documents (files) or tree (folder), empty when cancelled, null
     * when the picker couldn't be shown or the folder's access couldn't be kept. */
    private external fun onPicked(token: Long, uris: Array<String>?)

    private fun launch(activity: Activity, folder: Boolean) {
        val registry = (activity as? ComponentActivity)?.activityResultRegistry
        if (registry == null) {
            onPicked(token, null)
            return
        }
        val key = "margin-picker-" + next.incrementAndGet()
        try {
            if (folder) {
                lateinit var launcher: ActivityResultLauncher<Uri?>
                launcher = registry.register(key, ActivityResultContracts.OpenDocumentTree()) { uri ->
                    launcher.unregister()
                    when {
                        uri == null -> onPicked(token, emptyArray())
                        // The folder is held across restarts, so its grant is kept now,
                        // while the result's grant is live.
                        takePersistable(activity, uri.toString()) ->
                            onPicked(token, arrayOf(uri.toString()))
                        else -> onPicked(token, null)
                    }
                }
                launcher.launch(null)
            } else {
                lateinit var launcher: ActivityResultLauncher<Array<String>>
                launcher = registry.register(key, ActivityResultContracts.OpenMultipleDocuments()) { uris ->
                    launcher.unregister()
                    onPicked(token, uris.map { it.toString() }.toTypedArray())
                }
                launcher.launch(arrayOf("*/*"))
            }
        } catch (e: Exception) {
            Log.e(TAG, "couldn't show the picker", e)
            onPicked(token, null)
        }
    }

    companion object {
        private const val TAG = "MarginPicker"
        private const val EXTERNAL_STORAGE = "com.android.externalstorage.documents"
        private val next = AtomicInteger()

        /** Show the files picker (several at once) or the folder picker. */
        @JvmStatic
        fun pick(activity: Activity, folder: Boolean, token: Long) {
            val picker = MarginPicker(token)
            activity.runOnUiThread { picker.launch(activity, folder) }
        }

        /** A picked document's display name and size ("" when unknown). */
        @JvmStatic
        fun describe(context: Context, uri: String): Array<String> {
            val parsed = Uri.parse(uri)
            var name = (parsed.lastPathSegment ?: "").substringAfterLast('/')
            var size = ""
            try {
                val columns = arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)
                context.contentResolver.query(parsed, columns, null, null, null)?.use { c ->
                    if (c.moveToFirst()) {
                        if (!c.isNull(0)) name = c.getString(0)
                        if (!c.isNull(1)) size = c.getLong(1).toString()
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "couldn't describe $uri: $e")
            }
            return arrayOf(name, size)
        }

        // Held folders. Each call answers a status first ("ok", "revoked" when the
        // grant is gone, "notfound", "error"), then its data, or for a failure the
        // message.

        private fun failure(what: String, e: Exception): Array<String> {
            val status = when (e) {
                is SecurityException -> "revoked"
                is FileNotFoundException -> "notfound"
                else -> "error"
            }
            Log.w(TAG, "couldn't $what: $e")
            return arrayOf(status, e.message ?: e.toString())
        }

        private fun text(c: android.database.Cursor, i: Int): String =
            if (c.isNull(i)) "" else c.getString(i)

        private fun number(c: android.database.Cursor, i: Int): String =
            if (c.isNull(i)) "" else c.getLong(i).toString()

        /** A folder's own name from its document id ("primary:Documents/notes" is
         * "notes"); needs no access. */
        private fun nameFromId(id: String): String =
            id.substringAfterLast(':').trimEnd('/').substringAfterLast('/')

        /** Where a picked tree is, as the owner would recognise it: the storage or app
         * holding it, then the path inside it ("Internal storage/Documents/notes"). */
        private fun location(context: Context, treeUri: Uri, id: String, name: String): String {
            val authority = treeUri.authority ?: ""
            val place = when (authority) {
                EXTERNAL_STORAGE -> {
                    val volume = id.substringBefore(':', "")
                    if (volume == "primary" || volume.isEmpty()) "Internal storage" else "Storage $volume"
                }
                "com.android.providers.downloads.documents" -> "Downloads"
                "com.google.android.apps.docs.storage" -> "Google Drive"
                else -> try {
                    val pm = context.packageManager
                    @Suppress("DEPRECATION")
                    pm.resolveContentProvider(authority, 0)?.loadLabel(pm)?.toString()
                } catch (e: Exception) {
                    null
                } ?: authority
            }
            // Only external storage's ids are paths; elsewhere the folder's name is all
            // that's known.
            val inner = if (authority == EXTERNAL_STORAGE) {
                id.substringAfter(':', "").trim('/')
            } else {
                name
            }
            return if (inner.isEmpty()) place else "$place/$inner"
        }

        /**
         * A held tree's root folder: "ok", its document id, display name and location; or
         * a status and message, then the name its id gives (for the message shown when
         * access is lost).
         */
        @JvmStatic
        fun treeRoot(context: Context, tree: String): Array<String> {
            val treeUri: Uri
            val id: String
            try {
                treeUri = Uri.parse(tree)
                id = DocumentsContract.getTreeDocumentId(treeUri)
            } catch (e: Exception) {
                return failure("read the folder $tree", e) + ""
            }
            return try {
                val root = DocumentsContract.buildDocumentUriUsingTree(treeUri, id)
                var name = nameFromId(id)
                val columns = arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                val found = context.contentResolver.query(root, columns, null, null, null)?.use { c ->
                    if (!c.moveToFirst()) {
                        false
                    } else {
                        if (!c.isNull(0)) name = c.getString(0)
                        true
                    }
                } ?: false
                if (found) {
                    arrayOf("ok", id, name, location(context, treeUri, id, name))
                } else {
                    arrayOf("notfound", "The folder can't be found.", name)
                }
            } catch (e: Exception) {
                failure("open $tree", e) + nameFromId(id)
            }
        }

        /**
         * A document of a held tree: "ok", display name, MIME type, size and last
         * modified (milliseconds), "" for what the provider doesn't know.
         */
        @JvmStatic
        fun documentInfo(context: Context, tree: String, id: String): Array<String> {
            return try {
                val uri = DocumentsContract.buildDocumentUriUsingTree(Uri.parse(tree), id)
                val columns = arrayOf(
                    DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                    DocumentsContract.Document.COLUMN_MIME_TYPE,
                    DocumentsContract.Document.COLUMN_SIZE,
                    DocumentsContract.Document.COLUMN_LAST_MODIFIED,
                )
                context.contentResolver.query(uri, columns, null, null, null)?.use { c ->
                    if (c.moveToFirst()) {
                        arrayOf("ok", text(c, 0), text(c, 1), number(c, 2), number(c, 3))
                    } else {
                        null
                    }
                } ?: arrayOf("notfound", "There's no such document.")
            } catch (e: Exception) {
                failure("describe $id", e)
            }
        }

        /**
         * The documents in a folder of a held tree: "ok", then five strings each: document
         * id, display name, MIME type, size and last modified (milliseconds), "" for what
         * the provider doesn't know.
         */
        @JvmStatic
        fun children(context: Context, tree: String, parent: String): Array<String> {
            return try {
                val treeUri = Uri.parse(tree)
                val uri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, parent)
                val columns = arrayOf(
                    DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                    DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                    DocumentsContract.Document.COLUMN_MIME_TYPE,
                    DocumentsContract.Document.COLUMN_SIZE,
                    DocumentsContract.Document.COLUMN_LAST_MODIFIED,
                )
                val out = arrayListOf("ok")
                context.contentResolver.query(uri, columns, null, null, null)?.use { c ->
                    while (c.moveToNext()) {
                        if (c.isNull(0)) {
                            continue
                        }
                        out.add(c.getString(0))
                        out.add(text(c, 1))
                        out.add(text(c, 2))
                        out.add(number(c, 3))
                        out.add(number(c, 4))
                    }
                } ?: return arrayOf("notfound", "The folder can't be listed.")
                out.toTypedArray()
            } catch (e: Exception) {
                failure("list $parent", e)
            }
        }

        /** Open a document of a held tree for reading: "ok" and a file descriptor the
         * caller now owns. */
        @JvmStatic
        fun openRead(context: Context, tree: String, id: String): Array<String> {
            return try {
                val uri = DocumentsContract.buildDocumentUriUsingTree(Uri.parse(tree), id)
                val file = context.contentResolver.openFileDescriptor(uri, "r")
                    ?: return arrayOf("notfound", "The file couldn't be opened.")
                arrayOf("ok", file.detachFd().toString())
            } catch (e: Exception) {
                failure("open $id", e)
            }
        }

        /** Keep read access to a picked tree across restarts; false when it can't be kept. */
        @JvmStatic
        fun takePersistable(context: Context, tree: String): Boolean {
            return try {
                context.contentResolver.takePersistableUriPermission(
                    Uri.parse(tree),
                    Intent.FLAG_GRANT_READ_URI_PERMISSION,
                )
                true
            } catch (e: Exception) {
                Log.w(TAG, "couldn't keep access to $tree: $e")
                false
            }
        }

        /** Give up the read access kept for a tree (the folder was removed). */
        @JvmStatic
        fun releasePersistable(context: Context, tree: String) {
            try {
                context.contentResolver.releasePersistableUriPermission(
                    Uri.parse(tree),
                    Intent.FLAG_GRANT_READ_URI_PERMISSION,
                )
            } catch (e: Exception) {
                Log.w(TAG, "couldn't release $tree: $e")
            }
        }

        /** Copy a document into a file; null when it's copied, else what went wrong. */
        @JvmStatic
        fun copy(context: Context, uri: String, path: String): String? {
            return try {
                val input = context.contentResolver.openInputStream(Uri.parse(uri))
                    ?: return "The document couldn't be opened."
                input.use { from -> File(path).outputStream().use { to -> from.copyTo(to) } }
                null
            } catch (e: Exception) {
                e.message ?: e.toString()
            }
        }
    }
}
