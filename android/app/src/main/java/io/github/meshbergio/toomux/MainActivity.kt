package io.github.meshbergio.toomux

import android.annotation.SuppressLint
import android.app.Activity
import android.app.AlertDialog
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.io.ByteArrayInputStream
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.util.concurrent.Executors

/**
 * Android is a window onto the real Toomux shell.
 *
 * The ordinary app surface is the exact host TUI, rendered natively from an
 * ANSI cell grid. Touch becomes terminal mouse input and hardware/soft
 * keyboards become terminal keys. The separate WebView is used only for the
 * existing self-contained Toomux GUI memory explorer.
 */
class MainActivity : Activity(), TuiView.Listener {
    private lateinit var store: RemoteCredentialStore
    private lateinit var tui: TuiView
    private lateinit var memoryWeb: WebView
    private lateinit var pairCard: LinearLayout
    private lateinit var endpointEdit: EditText
    private lateinit var codeEdit: EditText
    private lateinit var pairButton: Button
    private lateinit var pairError: TextView
    private lateinit var connectionStatus: TextView

    private val main = Handler(Looper.getMainLooper())
    private val frameExecutor = Executors.newSingleThreadExecutor()
    private val inputExecutor = Executors.newSingleThreadExecutor()
    private var resumed = false
    private var memoryOpen = false
    private var credential: RemoteCredential? = null
    @Volatile private var cols = 120
    @Volatile private var rows = 40
    @Volatile private var lastFrame = ""
    @Volatile private var frameInFlight = false
    @Volatile private var memoryTuiOpen = false

    private val poll = Runnable { pollFrame() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        immersive()

        store = RemoteCredentialStore(this)
        credential = store.load()
        tui = findViewById<TuiView>(R.id.tui).also { it.listener = this }
        memoryWeb = findViewById(R.id.memory_web)
        pairCard = findViewById(R.id.pair_card)
        endpointEdit = findViewById(R.id.endpoint)
        codeEdit = findViewById(R.id.pair_code)
        pairButton = findViewById(R.id.pair_button)
        pairError = findViewById(R.id.pair_error)
        connectionStatus = findViewById(R.id.connection_status)

        endpointEdit.setText(credential?.endpoint ?: store.rememberedEndpoint())
        pairButton.setOnClickListener { pairDevice() }
        connectionStatus.setOnClickListener { onCommandPalette() }
        connectionStatus.setOnLongClickListener {
            confirmForget()
            true
        }

        configureMemoryWeb()
        renderMode()
        tui.post {
            val geometry = tui.currentGeometry()
            cols = geometry.first
            rows = geometry.second
            lastFrame = ""
        }
    }

    override fun onResume() {
        super.onResume()
        immersive()
        resumed = true
        scheduleFrame(0)
    }

    override fun onPause() {
        resumed = false
        main.removeCallbacks(poll)
        super.onPause()
    }

    override fun onDestroy() {
        frameExecutor.shutdownNow()
        inputExecutor.shutdownNow()
        memoryWeb.destroy()
        super.onDestroy()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) immersive()
    }

    @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
    override fun onBackPressed() {
        when {
            memoryOpen -> closeMemoryExplorer()
            credential != null -> sendKey("Escape")
            else -> super.onBackPressed()
        }
    }

    override fun onGeometry(cols: Int, rows: Int) {
        this.cols = cols
        this.rows = rows
        lastFrame = ""
        scheduleFrame(0)
    }

    override fun onTap(col: Int, row: Int, right: Boolean) {
        sendInput { it.tuiTap(col, row, right) }
    }

    override fun onScroll(col: Int, row: Int, delta: Int) {
        sendInput { it.tuiScroll(col, row, delta) }
    }

    override fun onKey(key: String) {
        sendKey(key)
    }

    override fun onText(text: String) {
        if (memoryTuiOpen && text == "o") {
            openMemoryExplorer()
        } else if (text.isNotEmpty()) {
            sendInput { it.tuiText(text) }
        }
    }

    override fun onCommandPalette() {
        if (memoryTuiOpen) {
            openMemoryExplorer()
        } else {
            // The control surface is Toomux's own help overlay, not a second
            // Android menu with a parallel command vocabulary.
            sendKey("?")
        }
    }

    private fun renderMode() {
        val paired = credential != null
        pairCard.visibility = if (paired) View.GONE else View.VISIBLE
        tui.visibility = if (paired && !memoryOpen) View.VISIBLE else View.GONE
        if (!paired) {
            memoryOpen = false
            memoryWeb.visibility = View.GONE
            connectionStatus.visibility = View.GONE
            lastFrame = ""
        }
    }

    private fun pairDevice() {
        pairError.visibility = View.GONE
        val code = codeEdit.text.toString().trim()
        if (code.length != 8 || code.any { !it.isDigit() }) {
            showPairError("Enter the eight-digit code from “toomux remote pair”.")
            return
        }

        val endpoint = endpointEdit.text.toString()
        val api = try {
            ToomuxApi(endpoint)
        } catch (e: IllegalArgumentException) {
            showPairError(e.message ?: "Invalid endpoint")
            return
        }

        pairButton.isEnabled = false
        frameExecutor.execute {
            try {
                val result = api.pair(code, deviceName())
                val next = RemoteCredential(api.endpoint, result.token, result.deviceId)
                store.save(next)
                main.post {
                    credential = next
                    codeEdit.text.clear()
                    pairButton.isEnabled = true
                    lastFrame = ""
                    renderMode()
                    toast("paired · Toomux is live")
                    scheduleFrame(0)
                }
            } catch (t: Throwable) {
                main.post {
                    pairButton.isEnabled = true
                    showPairError(friendly(t))
                }
            }
        }
    }

    private fun pollFrame() {
        if (!resumed || memoryOpen || frameInFlight) return
        val cred = credential ?: return
        frameInFlight = true
        val wantedCols = cols
        val wantedRows = rows
        val since = lastFrame

        frameExecutor.execute {
            try {
                val frame = ToomuxApi(cred.endpoint, cred.token).tuiFrame(
                    wantedCols,
                    wantedRows,
                    since.takeIf { it.isNotBlank() },
                )
                if (!frame.same || frame.sha256 != lastFrame) {
                    lastFrame = frame.sha256
                    main.post {
                        tui.applyFrame(frame)
                        memoryTuiOpen =
                            tui.containsText("memory ›") ||
                                (tui.containsText("memory index") && tui.containsText("◉ project"))
                        hideConnectionStatus()
                    }
                } else {
                    main.post { hideConnectionStatus() }
                }
            } catch (t: Throwable) {
                main.post { showConnectionStatus(friendly(t)) }
            } finally {
                frameInFlight = false
                main.post { scheduleFrame(FRAME_MS) }
            }
        }
    }

    private fun sendKey(key: String) {
        sendInput { it.tuiKey(key) }
    }

    private fun sendInput(action: (ToomuxApi) -> Unit) {
        val cred = credential ?: return
        inputExecutor.execute {
            try {
                action(ToomuxApi(cred.endpoint, cred.token))
                main.post { scheduleFrame(0) }
            } catch (t: Throwable) {
                main.post { showConnectionStatus(friendly(t)) }
            }
        }
    }

    private fun scheduleFrame(delayMs: Long) {
        main.removeCallbacks(poll)
        if (resumed && credential != null && !memoryOpen) {
            main.postDelayed(poll, delayMs)
        }
    }

    private fun restartRemoteTui() {
        val cred = credential ?: return
        showConnectionStatus("restarting Toomux…")
        inputExecutor.execute {
            runCatching { ToomuxApi(cred.endpoint, cred.token).closeTui() }
            lastFrame = ""
            main.post { scheduleFrame(0) }
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun configureMemoryWeb() {
        memoryWeb.setBackgroundColor(Color.rgb(15, 20, 28))
        memoryWeb.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = false
            allowFileAccess = false
            allowContentAccess = false
            builtInZoomControls = false
            displayZoomControls = false
            setSupportZoom(false)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
                mixedContentMode = android.webkit.WebSettings.MIXED_CONTENT_NEVER_ALLOW
            }
        }
        memoryWeb.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest?): Boolean = true

            override fun shouldInterceptRequest(
                view: WebView?,
                request: WebResourceRequest?,
            ): WebResourceResponse? {
                val scheme = request?.url?.scheme
                return if (scheme == "http" || scheme == "https") {
                    WebResourceResponse(
                        "text/plain",
                        "UTF-8",
                        ByteArrayInputStream(ByteArray(0)),
                    )
                } else {
                    super.shouldInterceptRequest(view, request)
                }
            }
        }
    }

    private fun openMemoryExplorer() {
        val cred = credential ?: return
        memoryOpen = true
        tui.hideKeyboard()
        tui.visibility = View.GONE
        memoryWeb.visibility = View.VISIBLE
        showConnectionStatus("building memory graph…")

        frameExecutor.execute {
            try {
                val html = ToomuxApi(cred.endpoint, cred.token).memoryPage()
                main.post {
                    if (!memoryOpen) return@post
                    memoryWeb.loadDataWithBaseURL(null, html, "text/html", "UTF-8", null)
                    hideConnectionStatus()
                }
            } catch (t: Throwable) {
                main.post {
                    closeMemoryExplorer()
                    showConnectionStatus(friendly(t))
                }
            }
        }
    }

    private fun closeMemoryExplorer() {
        memoryOpen = false
        memoryWeb.visibility = View.GONE
        tui.visibility = if (credential != null) View.VISIBLE else View.GONE
        immersive()
        scheduleFrame(0)
    }

    private fun confirmForget() {
        val cred = credential ?: return
        AlertDialog.Builder(this)
            .setTitle("Forget this device?")
            .setMessage("The host grant will be revoked when reachable and the local Keystore token will be removed.")
            .setNegativeButton("Cancel", null)
            .setPositiveButton("Forget") { _, _ ->
                inputExecutor.execute {
                    val revoked = runCatching {
                        ToomuxApi(cred.endpoint, cred.token).revokeSelf()
                    }.isSuccess
                    store.clear()
                    main.post {
                        credential = null
                        memoryOpen = false
                        memoryWeb.visibility = View.GONE
                        endpointEdit.setText(ToomuxApi.DEFAULT_ENDPOINT)
                        renderMode()
                        if (!revoked) {
                            toast("forgot locally · revoke " + cred.deviceId + " on the host when reachable")
                        }
                    }
                }
            }
            .show()
    }

    private fun showPairError(message: String) {
        pairError.text = message
        pairError.visibility = View.VISIBLE
    }

    private fun showConnectionStatus(message: String) {
        connectionStatus.text = message
        connectionStatus.visibility = View.VISIBLE
    }

    private fun hideConnectionStatus() {
        connectionStatus.visibility = View.GONE
    }

    private fun friendly(t: Throwable): String = when (t) {
        is ApiException -> t.message ?: "Toomux refused the request"
        is ConnectException,
        is NoRouteToHostException,
        is SocketTimeoutException,
        is UnknownHostException ->
            "ByteTraverse path unavailable · tap for controls"
        is IllegalArgumentException -> t.message ?: "Invalid setup"
        else -> t.message ?: "remote request failed"
    }

    private fun deviceName(): String =
        listOf(Build.MANUFACTURER, Build.MODEL)
            .filter { it.isNotBlank() }
            .joinToString(" ")
            .take(64)

    @Suppress("DEPRECATION")
    private fun immersive() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
            window.insetsController?.let {
                it.hide(WindowInsets.Type.statusBars() or WindowInsets.Type.navigationBars())
                it.systemBarsBehavior =
                    WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        } else {
            window.decorView.systemUiVisibility =
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or
                    View.SYSTEM_UI_FLAG_FULLSCREEN or
                    View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
                    View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
                    View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        }
    }

    private fun toast(message: String) =
        Toast.makeText(this, message, Toast.LENGTH_SHORT).show()

    companion object {
        private const val FRAME_MS = 140L
    }
}
