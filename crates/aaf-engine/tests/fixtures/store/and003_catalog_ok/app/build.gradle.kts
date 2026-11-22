plugins {
    alias(libs.plugins.android.application)
}

android {
    defaultConfig {
        targetSdk = libs.versions.android.targetSdk.get().toInt()
    }
}
