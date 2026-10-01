plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.github.meshbergio.toomux"
    compileSdk = 34

    defaultConfig {
        applicationId = "io.github.meshbergio.toomux"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        debug { isMinifyEnabled = false }
        release { isMinifyEnabled = false }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions { jvmTarget = "17" }
}

// Framework-only by design. ByteTraverse stays a separate VPN app/network
// substrate; no AGPL transport code is linked into the Toomux APK.
dependencies {
    testImplementation("junit:junit:4.13.2")
}
