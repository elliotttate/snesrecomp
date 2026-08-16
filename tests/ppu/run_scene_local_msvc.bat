@echo off
setlocal
call "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat" -arch=x64 -no_logo
if errorlevel 1 exit /b 1
cd /d "%~dp0\..\.."
if not exist build mkdir build
cl /nologo /W3 /O1 /DSNESRECOMP_REVERSE_DEBUG=0 ^
  /D_CRT_SECURE_NO_WARNINGS /Irunner\src /Irunner\src\snes ^
  tests\ppu\ws_shadow_scene_local_test.c ^
  runner\src\snes\ws_shadow.c ^
  /Fe:build\ws_shadow_scene_local_test_msvc.exe
if errorlevel 1 exit /b 1
build\ws_shadow_scene_local_test_msvc.exe
exit /b %errorlevel%
