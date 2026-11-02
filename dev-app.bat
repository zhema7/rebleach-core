@echo off
cd /d %~dp0
target\debug\rebleach.exe > logs_app.txt 2>&1
