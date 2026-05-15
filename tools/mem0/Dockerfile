FROM mem0/mem0-api-server:latest
# Install missing dependencies for the mem0 stack
RUN pip install psycopg2-binary langchain-neo4j rank-bm25 google-generativeai ollama groq mistralai && rm -rf /root/.cache