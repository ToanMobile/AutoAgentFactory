plugins {
    id("com.android.application")
}

android {
    compileSdk = (findProperty("androidCompileSdk") as String?)!!.toInt()
    defaultConfig {
        targetSdk = (findProperty("androidTargetSdk") as String?)!!.toInt()
    }
}
