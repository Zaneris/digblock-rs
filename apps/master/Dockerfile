# Build: docker build -f apps/master/Dockerfile -t digblock-master apps/master
# Linux + libmsquic is the well-trodden System.Net.Quic path, so the container
# probes for real (Master:Probe:Mode=Quic) — unlike bare macOS, where libmsquic
# is a manual brew install.

FROM mcr.microsoft.com/dotnet/sdk:10.0-alpine AS build
WORKDIR /src
COPY . .
RUN dotnet publish -c Release -o /out

FROM mcr.microsoft.com/dotnet/aspnet:10.0-alpine
RUN apk add --no-cache libmsquic
WORKDIR /app
COPY --from=build /out .
ENV ASPNETCORE_URLS=http://0.0.0.0:5200
# Accounts live here. In production /data MUST be a mounted volume (Azure
# Files), or a redeploy silently starts from an empty account database —
# the container's own layer is ephemeral. Created here so a plain
# `docker run` (dev, and the Rust e2e suite) still boots with no mount.
ENV Master__Db__Path=/data/master.db
RUN mkdir -p /data
VOLUME ["/data"]
EXPOSE 5200
ENTRYPOINT ["dotnet", "DigBlock.Master.dll"]
