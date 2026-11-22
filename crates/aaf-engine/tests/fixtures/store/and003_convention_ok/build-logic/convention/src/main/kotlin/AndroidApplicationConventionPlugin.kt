class AndroidApplicationConventionPlugin {
    fun configure(extension: ApplicationExtension) {
        extension.defaultConfig.targetSdk = libs.findVersion("targetSdk").get().toString().toInt()
    }
}
