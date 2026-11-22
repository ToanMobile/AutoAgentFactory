plugins {
    id("com.android.application")
}

val projectTargetSdk = (findProperty("androidTargetSdk") as String).toInt()

android {
    defaultConfig {
        targetSdk = projectTargetSdk
    }
}
