plugins {
    id("com.android.application")
}

android {
    defaultConfig {
        // targetSdk = 30
        /* targetSdk = 29
           targetSdkVersion(28) */
        targetSdk = 36 // was targetSdk = 31
        val note = "targetSdk = 27"
    }
}
