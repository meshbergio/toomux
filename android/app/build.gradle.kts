import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
}

val releaseKeystore = System.getenv("TOOMUX_ANDROID_KEYSTORE")
val releaseStorePassword = System.getenv("TOOMUX_ANDROID_STORE_PASSWORD")
val releaseKeyAlias = System.getenv("TOOMUX_ANDROID_KEY_ALIAS")
val releaseKeyPassword = System.getenv("TOOMUX_ANDROID_KEY_PASSWORD")
val hasReleaseSigning = listOf(
    releaseKeystore,
    releaseStorePassword,
    releaseKeyAlias,
    releaseKeyPassword,
).all { !it.isNullOrBlank() }
val hasAnyReleaseSigning = listOf(
    releaseKeystore,
    releaseStorePassword,
    releaseKeyAlias,
    releaseKeyPassword,
).any { !it.isNullOrBlank() }

check(!hasAnyReleaseSigning || hasReleaseSigning) {
    "Set all four TOOMUX_ANDROID_* signing variables, or none of them."
}

android {
    namespace = "io.github.meshbergio.toomux"
    compileSdk = 34

    defaultConfig {
        applicationId = "io.github.meshbergio.toomux"
        minSdk = 26
        targetSdk = 34
        versionCode = 5
        versionName = "0.2.3"
    }

    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = file(releaseKeystore!!)
                storePassword = releaseStorePassword!!
                keyAlias = releaseKeyAlias!!
                keyPassword = releaseKeyPassword!!
            }
        }
    }

    buildTypes {
        debug { isMinifyEnabled = false }
        release {
            isMinifyEnabled = false
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

// Framework-only by design. ByteTraverse stays a separate VPN app/network
// substrate; no AGPL transport code is linked into the Toomux APK.
dependencies {
    testImplementation("junit:junit:4.13.2")
}
