FROM debian:latest
ENV DEBIAN_FRONTEND=noninteractive

# Setup/Libs
RUN apt update && apt install -y python3 python3-pip && rm -rf /var/lib/apt/lists/*
RUN pip3 install ipgetter2 ipaddress

# Setup/Script
WORKDIR /workdir
COPY cname_switcher.py .

# Command
CMD python3 -u cname_switcher.py
