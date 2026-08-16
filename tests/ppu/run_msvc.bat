@echo off
setlocal
call "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat" -arch=x64 -no_logo
if errorlevel 1 exit /b 1
cd /d "%~dp0\..\.."
if not exist build mkdir build
cl /nologo /W3 /O1 /DSNESRECOMP_REVERSE_DEBUG=0 ^
  /D_CRT_SECURE_NO_WARNINGS /Irunner\src /Irunner\src\snes ^
  tests\ppu\ppu_sprite_limit_test.c ^
  runner\src\snes\ppu.c runner\src\snes\ppu_legacy.c ^
  /Fe:build\ppu_sprite_limit_test_msvc.exe
if errorlevel 1 exit /b 1
build\ppu_sprite_limit_test_msvc.exe
exit /b %errorlevel%
